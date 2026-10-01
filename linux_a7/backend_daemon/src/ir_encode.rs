/*
 * ir_encode.rs -- turns an IR code from the code library (issue #82,
 * ir_library.rs) into a code the IR blaster can send.
 *
 * The blaster speaks two codes (firmware_ir_blaster/PROTOCOL.md, "Codes"):
 *
 *   {"proto": "nec", "address": 0-65535, "command": 0-255}
 *   {"proto": "raw", "carrier_hz": 30000-60000, "timings": [us, ...]}
 *
 * The library (ir_library/irdb_import.py) already writes codes in
 * that form where it can. Everything else stays as the Flipper Zero
 * database parsed it, {"proto": "<name>", "address": N, "command": N}, and
 * is turned into raw timings HERE, on the hub: an encoder is a few lines
 * of Rust, testable on the PC, while a new protocol in the firmware would
 * mean updating every blaster.
 *
 * `to_blaster` returns None for a protocol without an encoder yet; the code
 * finder then leaves the sets that need it out.
 */
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

/* The blaster's limits for raw codes (PROTOCOL.md). */
const RAW_MAX_US: u64 = 32767;
const RAW_MAX_LEN: usize = 1024;

/* RC5 and RC6 have a TOGGLE bit that a real remote flips on every new
 * key press: the device tells "pressed again" from "still held" by it
 * (held Volume+ repeats; two presses of 1 are 11, not one 1). The hub
 * flips it on every send of such a code, like a remote. */
static TOGGLE: AtomicBool = AtomicBool::new(false);

/* A code -> what the blaster sends for one button press, or None if this
 * hub doesn't know the protocol (or the code is malformed). Taught codes
 * (already the blaster's) pass through; library codes are encoded here,
 * at every press, so the toggle bit changes. */
pub fn for_sending(code: &Value) -> Option<Value> {
    let toggles = matches!(code["proto"].as_str(), Some("rc5" | "rc5x" | "rc6"));
    let toggle = toggles && !TOGGLE.fetch_xor(true, Ordering::Relaxed);
    encode_code(code, toggle)
}

/* The same with the toggle bit clear: whether a code can be sent, and
 * what it looks like (the code finder compares codes with it). */
pub fn to_blaster(code: &Value) -> Option<Value> {
    encode_code(code, false)
}

fn encode_code(code: &Value, toggle: bool) -> Option<Value> {
    let proto = code["proto"].as_str()?;
    if proto == "raw" {
        let carrier = code["carrier_hz"].as_u64().filter(|c| (30000..=60000).contains(c))?;
        let timings = code["timings"].as_array()?;
        let ok = !timings.is_empty()
            && timings.len() <= RAW_MAX_LEN
            && timings.iter().all(|t| t.as_u64().is_some_and(|t| (1..=RAW_MAX_US).contains(&t)));
        return ok.then(|| json!({"proto": "raw", "carrier_hz": carrier, "timings": timings}));
    }
    /* Every other code is {proto, address, command}. */
    let address = code["address"].as_u64().filter(|a| *a <= u32::MAX as u64)? as u32;
    let command = code["command"].as_u64().filter(|c| *c <= u32::MAX as u64)? as u32;
    if proto == "nec" {
        /* The blaster's own NEC (standard or extended address). */
        return (address <= 0xFFFF && command <= 0xFF).then(|| json!({"proto": "nec", "address": address, "command": command}));
    }
    let (carrier, timings) = encode(proto, address, command, toggle)?;
    (timings.len() <= RAW_MAX_LEN).then(|| raw(carrier, timings))
}

fn raw(carrier_hz: u32, timings: Vec<u32>) -> Value {
    json!({"proto": "raw", "carrier_hz": carrier_hz, "timings": timings})
}

/* ------------------------------------------------------------------ */
/* The encoders                                                        */
/* ------------------------------------------------------------------ */

/* Each one sends what the Flipper Zero sends for the same parsed code
 * (its firmware, lib/infrared/encoder_decoder/, GPL-3.0 like this
 * project): the database's codes were recorded and named by Flippers, so
 * matching Flipper's encoder bit for bit is what makes them work. The
 * tests check each against real captures from Flipper's own unit tests.
 *
 * An IR frame is LED on (MARK) / LED off (SPACE) times. Two ways to put
 * bits into it:
 *
 *   pulse distance (NEC, Samsung, Kaseikyo, RCA, Pioneer): every bit is
 *     a short mark, and the SPACE after it says 0 (short) or 1 (long);
 *     one more mark at the end closes the last space.
 *   pulse width (Sony SIRC): the MARK's length says 0 or 1, spaces are
 *     all the same.
 *   Manchester / bi-phase (Philips RC5, RC6): every bit is two equal
 *     halves, one on and one off; the ORDER says 0 or 1.
 *
 * Bits go out lowest first from a byte array, as Flipper's encoder does
 * (protocols that send their fields highest bit first, RC5/RC6, have them
 * reversed while the array is built, exactly as Flipper does). */

/* A frame being built: (LED on?, microseconds). Neighbours with the same
 * level are merged when it becomes raw timings. */
type Frame = Vec<(bool, u32)>;

/* The timings of a pulse-distance or pulse-width protocol. */
struct Pulses {
    preamble: (u32, u32),
    zero: (u32, u32),
    one: (u32, u32),
}

/* The protocols, by the names the library uses (Flipper's, lowercased).
 * Returns the carrier and the raw timings. */
fn encode(proto: &str, address: u32, command: u32, toggle: bool) -> Option<(u32, Vec<u32>)> {
    const NEC: Pulses = Pulses { preamble: (9000, 4500), zero: (560, 560), one: (560, 1690) };
    let frame = match proto {
        /* Extended NEC: the 16-bit address and 16-bit command as given
         * (some remotes use the command's second byte freely). */
        "necext" => {
            let data = [address as u8, (address >> 8) as u8, command as u8, (command >> 8) as u8];
            (38000, pulse_distance(&NEC, &data, 32))
        }
        /* NEC with a 13-bit address: address, its inverse, command, its
         * inverse = 13 + 13 + 8 + 8 bits. */
        "nec42" => {
            let (a, c) = (address as u64 & 0x1FFF, command as u64 & 0xFF);
            let bits = a | ((!a & 0x1FFF) << 13) | (c << 26) | ((!c & 0xFF) << 34);
            (38000, pulse_distance(&NEC, &bits.to_le_bytes(), 42))
        }
        /* Samsung: like NEC, 4.5 ms leader, the address sent twice. */
        "samsung32" => {
            let (a, c) = (address as u8, command as u8);
            let p = Pulses { preamble: (4500, 4500), zero: (550, 550), one: (550, 1650) };
            (38000, pulse_distance(&p, &[a, a, c, !c], 32))
        }
        /* Panasonic and friends: 48 bits = vendor id (16), a parity
         * nibble, "genre" nibbles, a 12-bit command with a 2-bit id, and a
         * check byte. Flipper packs vendor/genre/id into the 32-bit
         * address: id in bits 24-25, vendor in 8-23, genres in 0-7. */
        "kaseikyo" => {
            let id = ((address >> 24) & 3) as u8;
            let vendor = ((address >> 8) & 0xFFFF) as u16;
            let (genre1, genre2) = (((address >> 4) & 0xF) as u8, (address & 0xF) as u8);
            let mut d = [vendor as u8, (vendor >> 8) as u8, 0, 0, 0, 0];
            let parity = d[0] ^ d[1];
            let parity = (parity & 0xF) ^ (parity >> 4);
            d[2] = (parity & 0xF) | (genre1 << 4);
            d[3] = genre2 | ((command as u8 & 0xF) << 4);
            d[4] = (id << 6) | ((command >> 4) as u8);
            d[5] = d[2] ^ d[3] ^ d[4];
            let p = Pulses { preamble: (3456, 1728), zero: (432, 432), one: (432, 1296) };
            (38000, pulse_distance(&p, &d, 48))
        }
        /* RCA: a 4-bit address and 8-bit command, then both inverted. */
        "rca" => {
            let (a, c) = (address & 0xF, command & 0xFF);
            let bits = a | (c << 4) | ((!a & 0xF) << 12) | ((!c & 0xFF) << 16);
            let p = Pulses { preamble: (4000, 4000), zero: (500, 1000), one: (500, 2000) };
            (38000, pulse_distance(&p, &bits.to_le_bytes(), 24))
        }
        /* Pioneer: NEC-like at 40 kHz, sent twice 26 ms apart -- Pioneer
         * devices want it twice. 32 bits as a real Pioneer remote sends
         * them (Flipper's encoder adds a 33rd, 0 bit; the captures of real
         * remotes in its own tests don't have it). */
        "pioneer" => {
            let (a, c) = (address as u8, command as u8);
            let p = Pulses { preamble: (8500, 4225), zero: (500, 500), one: (500, 1500) };
            let one = pulse_distance(&p, &[a, !a, c, !c], 32);
            (40000, repeated(&one, 2, |_| 26000))
        }
        /* Sony: pulse width; 7-bit command, then a 5-, 8- or 13-bit
         * address. Sony devices only react to the frame sent at least
         * three times, one every 45 ms. */
        "sirc" | "sirc15" | "sirc20" => {
            let (address_bits, n) = match proto {
                "sirc" => (5, 12),
                "sirc15" => (8, 15),
                _ => (13, 20),
            };
            let bits = (command & 0x7F) | ((address & ((1 << address_bits) - 1)) << 7);
            let p = Pulses { preamble: (2400, 600), zero: (600, 600), one: (1200, 600) };
            let one = pulse_width(&p, &bits.to_le_bytes(), n);
            (40000, repeated(&one, 3, |frame| 45000u32.saturating_sub(frame)))
        }
        /* Philips RC5 (and RC5X, 7-bit commands): 14 bi-phase bits, 889
         * us halves, no leader: 2 start bits (the second is "command
         * below 64" in RC5X), a toggle bit, 5 address and 6 command bits,
         * highest first; a 1 is off-then-on. */
        "rc5" | "rc5x" => {
            let mut bits: u16 = 0x01;
            if proto == "rc5" {
                bits |= 0x02;
            }
            if toggle {
                bits |= 0x04;
            }
            bits |= ((reverse(address as u8) >> 3) as u16) << 3;
            bits |= ((reverse(command as u8) >> 2) as u16) << 8;
            /* Flipper builds it with 1 = on-then-off, then inverts. */
            let bits = !bits;
            (36000, manchester(&bits.to_le_bytes(), 14, 888, None, None))
        }
        /* Philips RC6 (mode 0): 2666/889 us leader, then 21 bi-phase bits
         * of 444 us halves: start bit, 3 mode bits (0), the toggle bit
         * (twice as long: the "trailer"), 8 address and 8 command bits,
         * highest first; a 1 is on-then-off. */
        "rc6" => {
            let bits: u32 =
                0x01 | ((toggle as u32) << 4) | ((reverse(address as u8) as u32) << 5) | ((reverse(command as u8) as u32) << 13);
            (36000, manchester(&bits.to_le_bytes(), 21, 444, Some((2666, 889)), Some(4)))
        }
        _ => return None,
    };
    let (carrier, frame) = frame;
    Some((carrier, timings(&frame)))
}

/* Pulse distance: per bit a mark and a 0/1 space, then a closing mark
 * (Flipper: the next bit's mark, a 0 bit past the end). */
fn pulse_distance(p: &Pulses, data: &[u8], n: usize) -> Frame {
    let mut f = vec![(true, p.preamble.0), (false, p.preamble.1)];
    for i in 0..n {
        let (mark, space) = if bit(data, i) { p.one } else { p.zero };
        f.push((true, mark));
        f.push((false, space));
    }
    f.push((true, p.zero.0));
    f
}

/* Pulse width: per bit a 0/1 mark, with equal spaces in between. */
fn pulse_width(p: &Pulses, data: &[u8], n: usize) -> Frame {
    let mut f = vec![(true, p.preamble.0), (false, p.preamble.1)];
    for i in 0..n {
        if i > 0 {
            f.push((false, p.zero.1));
        }
        f.push((true, if bit(data, i) { p.one.0 } else { p.zero.0 }));
    }
    f
}

/* Bi-phase: each bit = two halves, the first ON for a 1. `double`: the
 * bit whose halves are twice as long (RC6's toggle bit). The last bit's
 * second half is left out when it would be "off" (nothing follows). */
fn manchester(data: &[u8], n: usize, half: u32, preamble: Option<(u32, u32)>, double: Option<usize>) -> Frame {
    let mut f = Frame::new();
    if let Some((mark, space)) = preamble {
        f.push((true, mark));
        f.push((false, space));
    }
    for i in 0..n {
        let t = if double == Some(i) { 2 * half } else { half };
        let one = bit(data, i);
        f.push((one, t));
        if !(one && i + 1 == n) {
            f.push((!one, t));
        }
    }
    f
}

/* `count` copies of a frame, each followed by the pause `gap` gives for
 * the frame's length (none after the last). */
fn repeated(frame: &Frame, count: usize, gap: impl Fn(u32) -> u32) -> Frame {
    let length: u32 = frame.iter().map(|(_, t)| t).sum();
    let mut f = Frame::new();
    for i in 0..count {
        if i > 0 {
            f.push((false, gap(length)));
        }
        f.extend_from_slice(frame);
    }
    f
}

/* Bit i of a byte array, lowest bit of the first byte first. */
fn bit(data: &[u8], i: usize) -> bool {
    data.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
}

fn reverse(b: u8) -> u8 {
    b.reverse_bits()
}

/* A frame -> the blaster's raw timings: neighbours with the same level
 * merged, starting with a mark (a leading "off" is just silence) and
 * ending with one. */
fn timings(frame: &Frame) -> Vec<u32> {
    let mut out: Vec<(bool, u32)> = Vec::new();
    for &(on, t) in frame {
        match out.last_mut() {
            Some((level, total)) if *level == on => *total += t,
            Some(_) => out.push((on, t)),
            None if on => out.push((on, t)),
            None => {}
        }
    }
    while out.last().is_some_and(|(on, _)| !on) {
        out.pop();
    }
    out.into_iter().map(|(_, t)| t.min(RAW_MAX_US as u32)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blaster_codes_pass_through() {
        let nec = json!({"proto": "nec", "address": 0, "command": 64});
        assert_eq!(to_blaster(&nec), Some(nec.clone()));
        let raw = json!({"proto": "raw", "carrier_hz": 38000, "timings": [9000, 4500, 560]});
        assert_eq!(to_blaster(&raw), Some(raw.clone()));
    }

    #[test]
    fn bad_codes_are_refused() {
        assert_eq!(to_blaster(&json!({"proto": "nec", "address": 0, "command": 300})), None);
        assert_eq!(to_blaster(&json!({"proto": "raw", "carrier_hz": 455000, "timings": [500]})), None);
        assert_eq!(to_blaster(&json!({"proto": "raw", "carrier_hz": 38000, "timings": [40000]})), None);
        assert_eq!(to_blaster(&json!({"proto": "raw", "carrier_hz": 38000, "timings": []})), None);
        assert_eq!(to_blaster(&json!({"command": 1})), None);
    }

    #[test]
    fn unknown_protocols_wait_for_an_encoder() {
        assert_eq!(to_blaster(&json!({"proto": "nec42ext", "address": 7, "command": 2})), None);
    }

    /* Real remotes recorded in the Flipper Zero firmware's unit tests
     * (applications/debug/unit_tests/resources/unit_tests/infrared,
     * GPL-3.0): (protocol, address, command, toggle bit, one frame). The
     * whole set is checked by flipper::captures below. */
    const CAPTURED: &[(&str, u32, u32, bool, &[u32])] = &[
        ("kaseikyo", 0x325441, 0x1b, false, &[3363, 1685, 407, 436, 411, 432, 415, 1240, 434, 410, 437, 1245, 439, 404, 433, 1249, 435, 408, 439, 431, 406, 1249, 435, 435, 412, 405, 442, 1241, 433, 1249, 435, 408, 439, 405, 442, 428, 409, 434, 413, 430, 407, 411, 436, 433, 414, 429, 408, 1248, 436, 407, 440, 1243, 441, 428, 409, 434, 413, 431, 406, 1249, 435, 1248, 436, 406, 441, 1242, 442, 1240, 434, 409, 438, 431, 416, 428, 409, 408, 439, 430, 407, 411, 436, 407, 440, 429, 408, 436, 411, 432, 415, 402, 435, 1247, 437, 1245, 439, 1243, 441, 1238, 436]),
        ("necext", 0x286, 0xb649, false, &[8862, 4452, 562, 563, 559, 1681, 563, 1646, 567, 586, 556, 569, 563, 583, 559, 571, 561, 1675, 559, 565, 567, 1673, 561, 561, 561, 592, 561, 565, 567, 579, 563, 567, 565, 584, 558, 1652, 561, 592, 561, 561, 561, 1679, 565, 560, 562, 584, 558, 1659, 564, 585, 557, 566, 566, 1675, 559, 1649, 564, 589, 564, 1649, 564, 1668, 566, 565, 567, 1669, 565]),
        ("necext", 0x7984, 0xed12, false, &[8967, 4463, 587, 527, 590, 524, 584, 1647, 590, 524, 583, 531, 586, 527, 590, 524, 583, 1646, 589, 1640, 586, 527, 590, 524, 583, 1647, 590, 1640, 587, 1644, 582, 1647, 589, 524, 583, 531, 586, 1644, 593, 521, 586, 527, 589, 1641, 586, 528, 589, 525, 592, 521, 585, 1644, 592, 522, 585, 1645, 592, 1638, 589, 524, 592, 1637, 588, 1641, 585, 1645, 592]),
        ("pioneer", 0xaf, 0x36, false, &[8437, 4188, 571, 1538, 595, 1514, 567, 1542, 570, 1539, 573, 501, 565, 1544, 568, 506, 571, 1539, 573, 501, 565, 509, 568, 508, 569, 506, 571, 1538, 574, 501, 565, 1543, 569, 506, 571, 504, 573, 1536, 566, 1544, 568, 506, 592, 1517, 574, 1535, 567, 507, 570, 505, 572, 1537, 575, 500, 566, 508, 600, 1509, 572, 503, 574, 501, 565, 1544, 568, 1540, 593]),
        ("rc5", 0x13, 0x10, false, &[888, 888, 1776, 1776, 1776, 888, 888, 1776, 888, 888, 1776, 1776, 1776, 888, 888, 888, 888, 888, 888]),
        ("rc5x", 0x13, 0x10, false, &[1776, 888, 888, 1776, 1776, 888, 888, 1776, 888, 888, 1776, 1776, 1776, 888, 888, 888, 888, 888, 888]),
        ("rc6", 0x94, 0xa0, false, &[2666, 889, 444, 888, 444, 444, 444, 444, 444, 888, 1332, 888, 444, 444, 888, 888, 888, 888, 444, 444, 888, 888, 888, 888, 444, 444, 444, 444, 444, 444, 444, 444, 444]),
        ("rca", 0xf, 0x54, false, &[3994, 3969, 552, 1945, 551, 1945, 552, 1945, 551, 1945, 552, 946, 551, 947, 550, 1947, 548, 951, 546, 1953, 542, 979, 518, 1979, 517, 981, 492, 1006, 491, 1006, 492, 1006, 492, 1006, 492, 2005, 492, 2005, 492, 1006, 492, 2005, 492, 1006, 492, 2005, 492, 1006, 492, 2006, 491]),
        ("samsung32", 0xe, 0xc, false, &[4513, 4483, 565, 530, 586, 1670, 563, 1664, 588, 1666, 566, 530, 586, 535, 560, 535, 591, 531, 565, 531, 585, 1669, 563, 1666, 587, 1640, 593, 531, 566, 530, 587, 536, 559, 562, 564, 531, 585, 537, 558, 1670, 562, 1665, 587, 534, 561, 534, 592, 530, 566, 529, 587, 1668, 564, 1664, 589, 533, 563, 533, 594, 1661, 560, 1667, 565, 1661, 591, 1664, 558]),
        ("sirc", 0x10, 0x15, false, &[2420, 608, 1194, 608, 596, 604, 1198, 603, 591, 610, 1192, 609, 596, 605, 599, 601, 593, 607, 597, 604, 590, 610, 594, 606, 1196]),
        ("rc6", 0x93, 0xa1, true, &[2666, 889, 444, 888, 444, 444, 444, 444, 1332, 888, 444, 888, 444, 444, 888, 888, 444, 444, 888, 444, 444, 444, 444, 888, 888, 888, 444, 444, 444, 444, 444, 444, 888]),
    ];

    #[test]
    fn encoders_match_real_remotes() {
        for (proto, address, command, toggle, frame) in CAPTURED {
            let (_, timings) = encode(proto, *address, *command, *toggle).unwrap();
            let mine = super::flipper::first_frame(&timings);
            assert!(super::flipper::matches(&mine, frame), "{proto} {address:#x} {command:#x}:\n mine {mine:?}\n real {frame:?}");
        }
    }

    #[test]
    fn necext_with_a_free_command_byte() {
        /* Address 0x0005 (bytes 05 00), command bytes 03 12 (12 isn't
         * ~03): only raw can carry it. */
        let code = to_blaster(&json!({"proto": "necext", "address": 5, "command": 0x1203})).unwrap();
        assert_eq!(code["carrier_hz"], 38000);
        let t: Vec<u32> = code["timings"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
        assert_eq!(read_bits(&t, 32), 0x1203_0005);
    }

    #[test]
    fn nec42_layout() {
        /* No recording in Flipper's tests: read the bits back. 13-bit
         * address, its inverse, command, its inverse. */
        let (_, t) = encode("nec42", 0x1ABC, 0x5A, false).unwrap();
        let bits = read_bits(&t, 42);
        assert_eq!(bits & 0x1FFF, 0x1ABC);
        assert_eq!((bits >> 13) & 0x1FFF, !0x1ABC & 0x1FFF);
        assert_eq!((bits >> 26) & 0xFF, 0x5A);
        assert_eq!((bits >> 34) & 0xFF, 0xA5);
    }

    #[test]
    fn sony_is_sent_three_times() {
        let (carrier, t) = encode("sirc", 1, 0x15, false).unwrap();
        assert_eq!(carrier, 40000);
        /* 3 frames of 2 + 12 * 2 - 1 timings, and 2 pauses between. */
        assert_eq!(t.len(), 3 * 25 + 2);
        /* One frame every 45 ms. */
        let first: u32 = t[..26].iter().sum();
        assert_eq!(first, 45000);
    }

    #[test]
    fn toggle_flips_on_every_press() {
        let code = json!({"proto": "rc5", "address": 0x13, "command": 0x10});
        let (a, b) = (for_sending(&code).unwrap(), for_sending(&code).unwrap());
        assert_ne!(a, b);
        assert!(a == to_blaster(&code).unwrap() || b == to_blaster(&code).unwrap());
        /* Taught codes are sent as they are. */
        let nec = json!({"proto": "nec", "address": 0, "command": 64});
        assert_eq!(for_sending(&nec), Some(nec.clone()));
    }

    /* Pulse-distance timings (leader first) -> the bits, lowest first. */
    fn read_bits(t: &[u32], n: usize) -> u64 {
        (0..n).fold(0, |bits, i| bits | (((t[3 + 2 * i] > 1000) as u64) << i))
    }
}

/* Against the Flipper Zero firmware's own IR unit tests (recorded remotes
 * and the codes they decode to), e.g.:
 *   FLIPPER_IRTEST_DIR=.../applications/debug/unit_tests/resources/unit_tests/infrared \
 *     cargo test ir_encode::flipper -- --ignored --nocapture */
#[cfg(test)]
pub mod flipper {
    use super::*;

    /* The frames of a capture: split at every space longer than 5 ms (no
     * protocol here has a longer one inside a frame). The capture starts
     * with a space (the silence before it). */
    pub fn frames(capture: &[u32]) -> Vec<Vec<u32>> {
        let mut frames = Vec::new();
        let mut cur = Vec::new();
        for (i, &t) in capture.iter().enumerate() {
            let mark = i % 2 == 1;
            if !mark && t > 5000 {
                if !cur.is_empty() {
                    frames.push(std::mem::take(&mut cur));
                }
            } else if mark || !cur.is_empty() {
                cur.push(t);
            }
        }
        if !cur.is_empty() {
            frames.push(cur);
        }
        frames
    }

    /* Same number of timings, each within 25 % (or 150 us). */
    pub fn matches(encoded: &[u32], captured: &[u32]) -> bool {
        encoded.len() == captured.len()
            && encoded.iter().zip(captured).all(|(&e, &c)| e.abs_diff(c) <= (e / 4).max(150))
    }

    /* The encoded code split the same way (SIRC, Pioneer: several frames). */
    pub fn first_frame(timings: &[u32]) -> Vec<u32> {
        let mut with_silence = vec![100_000];
        with_silence.extend_from_slice(timings);
        frames(&with_silence).remove(0)
    }

    struct Message {
        proto: String,
        address: u32,
        command: u32,
    }

    fn le(text: &str) -> u32 {
        text.split_whitespace().enumerate().map(|(i, b)| u32::from_str_radix(b, 16).unwrap() << (8 * i)).sum()
    }

    #[test]
    #[ignore]
    fn captures() {
        let dir = std::env::var("FLIPPER_IRTEST_DIR").expect("FLIPPER_IRTEST_DIR");
        let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        let mut failed = 0;
        for path in entries {
            let text = std::fs::read_to_string(&path).unwrap();
            /* name -> raw data, and name -> expected messages. */
            let mut inputs: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
            let mut expected: std::collections::BTreeMap<String, Vec<Message>> = Default::default();
            let mut name = String::new();
            let mut msg: Option<Message> = None;
            for line in text.lines() {
                let (key, value) = line.split_once(':').map(|(k, v)| (k.trim(), v.trim())).unwrap_or(("", ""));
                match key {
                    "name" => name = value.to_string(),
                    "data" if name.starts_with("decoder_input") => {
                        inputs.insert(name.clone(), value.split_whitespace().map(|t| t.parse().unwrap()).collect());
                    }
                    "protocol" => msg = Some(Message { proto: value.to_lowercase(), address: 0, command: 0 }),
                    "address" => msg.as_mut().unwrap().address = le(value),
                    "command" => msg.as_mut().unwrap().command = le(value),
                    "repeat" => {
                        let m = msg.take().unwrap();
                        if value == "false" && name.starts_with("decoder_expected") {
                            expected.entry(name.replace("expected", "input")).or_default().push(m);
                        }
                    }
                    _ => {}
                }
            }
            let (mut ok, mut bad, mut unknown) = (0, 0, 0);
            for (input, messages) in &expected {
                let Some(capture) = inputs.get(input) else { continue };
                let captured = frames(capture);
                /* Made-up frames that test Flipper's DECODER on odd
                 * input (extra pulses), not real remotes. */
                if path.ends_with("test_sirc.irtest") && input == "decoder_input2" {
                    continue;
                }
                for m in messages {
                    let Some((_, t)) = encode(&m.proto, m.address, m.command, false) else {
                        unknown += 1;
                        continue;
                    };
                    let mine = first_frame(&t);
                    /* A capture may have the toggle bit either way. */
                    let toggled = encode(&m.proto, m.address, m.command, true).map(|(_, t)| first_frame(&t));
                    if let Some(f) = captured.iter().find(|f| matches(&mine, f) || toggled.as_ref().is_some_and(|t| matches(t, f))) {
                        if ok == 0 && std::env::var("PRINT_VECTORS").is_ok() {
                            let toggle = !matches(&mine, f);
                            println!("VECTOR (\"{}\", {:#x}, {:#x}, {toggle}, &{f:?}),", m.proto, m.address, m.command);
                        }
                        ok += 1;
                    } else {
                        bad += 1;
                        if bad <= 2 {
                            println!("  MISMATCH {} {:#x} {:#x}: {:?}", m.proto, m.address, m.command, mine);
                            for f in captured.iter().filter(|f| f.len() == mine.len()).take(2) {
                                println!("     same length captured: {f:?}");
                            }
                        }
                    }
                }
            }
            failed += bad;
            println!("{}: {ok} match, {bad} differ, {unknown} not encoded", path.file_name().unwrap().to_string_lossy());
        }
        assert_eq!(failed, 0);
    }
}
