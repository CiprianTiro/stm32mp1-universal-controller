/*
 * pb.rs -- just enough Protocol Buffers for Espressif's provisioning
 * messages (issue #42).
 *
 * Their messages (session.proto, sec2.proto, network_config.proto) are a
 * handful of small structures made of three things: numbers (varints),
 * byte strings, and nested messages. Writing and reading those by hand is a
 * few dozen lines, instead of a protobuf compiler and generated code in the
 * build. Wire format: each field is a key (field number << 3 | wire type),
 * then the value: wire type 0 = varint, 2 = length + bytes (also used for
 * nested messages and strings).
 *
 * One proto3 rule matters when READING: a number that is 0 is not sent at
 * all. So a missing field means 0 -- e.g. a missing wifi_sta_state means
 * "Connected" (= 0), not "unknown".
 */

/* Builds one message, field by field. */
#[derive(Default)]
pub struct Msg {
    buf: Vec<u8>,
}

fn varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

impl Msg {
    pub fn new() -> Msg {
        Msg::default()
    }

    /* A number (enum, int). 0 is left out, as proto3 does. */
    pub fn int(mut self, field: u32, value: u64) -> Msg {
        if value != 0 {
            varint(&mut self.buf, u64::from(field) << 3);
            varint(&mut self.buf, value);
        }
        self
    }

    /* A byte string (or text). */
    pub fn bytes(mut self, field: u32, value: &[u8]) -> Msg {
        varint(&mut self.buf, u64::from(field) << 3 | 2);
        varint(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(value);
        self
    }

    /* A nested message -- also when it's empty: its presence is what says
     * which kind of payload this is (a "oneof"). */
    pub fn msg(self, field: u32, inner: Msg) -> Msg {
        self.bytes(field, &inner.buf)
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/* One received message: its fields, in order. */
pub struct Fields(Vec<(u32, Value)>);

enum Value {
    Int(u64),
    Bytes(Vec<u8>),
}

impl Fields {
    /* Parses a message; Err for anything malformed (this is data from a
     * device over the air: never trusted to be well-formed). */
    pub fn parse(data: &[u8]) -> Result<Fields, String> {
        let mut fields = Vec::new();
        let mut pos = 0;
        let read_varint = |pos: &mut usize| -> Result<u64, String> {
            let mut value = 0u64;
            for shift in (0..64).step_by(7) {
                let byte = *data.get(*pos).ok_or("message cut short")?;
                *pos += 1;
                value |= u64::from(byte & 0x7F) << shift;
                if byte & 0x80 == 0 {
                    return Ok(value);
                }
            }
            Err("number too long".into())
        };
        while pos < data.len() {
            let key = read_varint(&mut pos)?;
            let field = u32::try_from(key >> 3).map_err(|_| "bad field number")?;
            match key & 7 {
                0 => fields.push((field, Value::Int(read_varint(&mut pos)?))),
                2 => {
                    let len = usize::try_from(read_varint(&mut pos)?).map_err(|_| "bad length")?;
                    let end = pos.checked_add(len).filter(|&e| e <= data.len()).ok_or("message cut short")?;
                    fields.push((field, Value::Bytes(data[pos..end].to_vec())));
                    pos = end;
                }
                other => return Err(format!("unsupported field type {other}")),
            }
        }
        Ok(Fields(fields))
    }

    /* A number field; 0 if absent (proto3). */
    pub fn int(&self, field: u32) -> u64 {
        self.0
            .iter()
            .rev()
            .find_map(|(f, v)| match v {
                Value::Int(i) if *f == field => Some(*i),
                _ => None,
            })
            .unwrap_or(0)
    }

    /* A bytes field; empty if absent. */
    pub fn bytes(&self, field: u32) -> Vec<u8> {
        self.get(field).unwrap_or_default()
    }

    /* A bytes (or nested message) field, None if absent. */
    pub fn get(&self, field: u32) -> Option<Vec<u8>> {
        self.0.iter().rev().find_map(|(f, v)| match v {
            Value::Bytes(b) if *f == field => Some(b.clone()),
            _ => None,
        })
    }

    /* A nested message; an empty one if absent. */
    pub fn msg(&self, field: u32) -> Result<Fields, String> {
        Fields::parse(&self.bytes(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_proto3_defaults() {
        let data = Msg::new()
            .int(2, 2)
            .msg(12, Msg::new().int(1, 0).msg(20, Msg::new().bytes(1, b"wifiprov").bytes(2, &[0xAB; 300])))
            .build();
        let top = Fields::parse(&data).unwrap();
        assert_eq!(top.int(2), 2);
        let sec2 = top.msg(12).unwrap();
        assert_eq!(sec2.int(1), 0); /* left out, reads as 0 */
        let sc0 = sec2.msg(20).unwrap();
        assert_eq!(sc0.bytes(1), b"wifiprov");
        assert_eq!(sc0.bytes(2).len(), 300); /* a 2-byte length */
        assert!(top.get(99).is_none());
    }

    /* The exact bytes Espressif's generated Python classes produce (run
     * with protobuf's SerializeToString): SessionData { sec_ver 2, sec2 {
     * msg Command1, sc1 { client_proof aa bb } } }, and the two WiFi
     * commands with an empty payload message. */
    #[test]
    fn matches_python_protobuf_bytes() {
        let data = Msg::new()
            .int(2, 2)
            .msg(12, Msg::new().int(1, 2).msg(22, Msg::new().bytes(1, &[0xAA, 0xBB])))
            .build();
        assert_eq!(data, [0x10, 0x02, 0x62, 0x09, 0x08, 0x02, 0xb2, 0x01, 0x04, 0x0a, 0x02, 0xaa, 0xbb]);
        /* apply: msg 4, cmd_apply_wifi_config (14) empty */
        assert_eq!(Msg::new().int(1, 4).msg(14, Msg::new()).build(), [0x08, 0x04, 0x72, 0x00]);
        /* status: msg 0 (left out), cmd_get_wifi_status (10) empty */
        assert_eq!(Msg::new().int(1, 0).msg(10, Msg::new()).build(), [0x52, 0x00]);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(Fields::parse(&[0x0a, 0x05, 0x01]).is_err()); /* length past the end */
        assert!(Fields::parse(&[0x08]).is_err()); /* varint missing */
        assert!(Fields::parse(&[0x0b]).is_err()); /* wire type 3 */
        assert!(Fields::parse(&[0xff; 12]).is_err()); /* endless varint */
    }
}
