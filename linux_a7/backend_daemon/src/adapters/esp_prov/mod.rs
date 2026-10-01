/*
 * esp_prov -- the hub setting up a device's WiFi over Bluetooth, with
 * Espressif's standard provisioning protocol (issue #42; the device side is
 * firmware_ir_blaster/main/provision.c, Espressif's network_provisioning
 * component).
 *
 * The same thing Espressif's esp_prov tool and phone app do, as a hub
 * feature: the hub is the "phone", the device a factory-fresh ESP32.
 *
 *   proto-ver     "---"  -> {"prov": {"sec_ver": 2, "sec_patch_ver": 1, ...}}
 *   prov-session  SRP6a with the device's pairing code (srp.rs): two round
 *                 trips, then both sides share a key. A wrong code is
 *                 refused here, before anything secret is sent.
 *   prov-config   encrypted (AES-256-GCM): the WiFi name and password,
 *                 "apply", then "status?" until Connected (with the
 *                 device's new IP address) or failed (wrong password /
 *                 network not found)
 *
 * The transport (Bluetooth GATT, ble.rs) is behind the `Transport` trait:
 * write a request to an endpoint, read the answer. That keeps this file
 * free of Bluetooth details, and lets the tests run the whole exchange
 * against a simulated device.
 */
pub mod ble;
pub mod pb;
pub mod srp;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use std::time::Duration;

use super::BoxFuture;
use pb::{Fields, Msg};

/* SRP6a user name: Espressif's default, as the firmware uses it. */
pub const USERNAME: &str = "wifiprov";

/* How often to ask "status?", and how long to wait for the device to join
 * the WiFi (it tries a few times before saying "failed"). */
#[derive(Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub join_timeout: Duration,
}

pub const TIMING: Timing = Timing {
    poll: Duration::from_secs(1),
    join_timeout: Duration::from_secs(40),
};

/* Equal, in constant time: how long it takes doesn't reveal where two
 * proofs first differ. */
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/* Write a request to an endpoint, read its answer. */
pub trait Transport: Send {
    fn exchange<'a>(&'a mut self, endpoint: &'a str, data: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

/* Why setting up failed, as the wizard needs to tell it. */
#[derive(Debug, PartialEq)]
pub enum ProvError {
    /* The device refused the pairing code. */
    WrongCode,
    /* The device couldn't join: the WiFi password is wrong. */
    WifiPassword,
    /* The device couldn't join: it doesn't see that network (a 5 GHz-only
     * network, a typo in the name, out of range). */
    WifiNotFound,
    /* No "connected" in JOIN_TIMEOUT. */
    Timeout,
    /* Bluetooth, or a device that answers something unexpected. */
    Link(String),
}

impl std::fmt::Display for ProvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvError::WrongCode => f.write_str("the device refused the pairing code"),
            ProvError::WifiPassword => f.write_str("the device couldn't join the WiFi: wrong password"),
            ProvError::WifiNotFound => f.write_str("the device doesn't see that WiFi network (it needs 2.4 GHz)"),
            ProvError::Timeout => f.write_str("the device didn't report joining the WiFi in time"),
            ProvError::Link(e) => write!(f, "Bluetooth: {e}"),
        }
    }
}

fn link(e: impl Into<String>) -> ProvError {
    ProvError::Link(e.into())
}

/* An established session: requests to prov-config go encrypted. */
pub struct Session<T: Transport> {
    transport: T,
    key: LessSafeKey,
    nonce: [u8; 12],
    /* sec_patch_ver 1: the nonce's last 4 bytes count up after every
     * message, each direction (a nonce must never be used twice with the
     * same key). Patch 0 devices use one nonce throughout. */
    counting: bool,
}

/* The device's answer to "status?". */
#[derive(Debug, PartialEq)]
pub enum WifiState {
    Connected { ip: String },
    Connecting,
    Failed(ProvError),
}

impl<T: Transport> Session<T> {
    /* Opens a session with the device's pairing code (as the device keeps
     * it: 16 characters, upper case, no dashes). */
    pub async fn open(mut transport: T, code: &str) -> Result<Session<T>, ProvError> {
        /* Which security the device wants (sec_patch_ver: nonce counting). */
        let version = transport.exchange("proto-ver", b"---").await.map_err(link)?;
        let version: serde_json::Value =
            serde_json::from_slice(&version).map_err(|e| link(format!("bad version answer: {e}")))?;
        if version["prov"]["sec_ver"] != serde_json::json!(2) {
            return Err(link(format!("the device doesn't use security 2: {version}")));
        }
        let counting = version["prov"]["sec_patch_ver"].as_u64().unwrap_or(0) >= 1;

        /* SRP6a, round 1: our public value, the device's value and salt. */
        let rng = SystemRandom::new();
        let client = srp::Client::new(USERNAME, code, |buf: &mut [u8]| {
            rng.fill(buf).expect("the system's random generator works");
        });
        let cmd0 = Msg::new()
            .int(2, 2)
            .msg(12, Msg::new().int(1, 0).msg(20, Msg::new().bytes(1, USERNAME.as_bytes()).bytes(2, &client.public)))
            .build();
        let resp0 = Fields::parse(&transport.exchange("prov-session", &cmd0).await.map_err(link)?).map_err(link)?;
        let sec2 = resp0.msg(12).map_err(link)?;
        let sr0 = sec2.msg(21).map_err(link)?;
        if sec2.int(1) != 1 || sr0.int(1) != 0 {
            return Err(link("the device refused to start a session"));
        }
        let challenge = client
            .challenge(&sr0.bytes(3), &sr0.bytes(2))
            .ok_or_else(|| link("the device sent an invalid SRP value"))?;

        /* Round 2: our proof. A device that doesn't accept it (wrong code)
         * drops the connection instead of answering -- that's what the
         * ESP32 does -- so a failed exchange HERE means "wrong code". */
        let cmd1 = Msg::new()
            .int(2, 2)
            .msg(12, Msg::new().int(1, 2).msg(22, Msg::new().bytes(1, &challenge.proof)))
            .build();
        let resp1 = transport.exchange("prov-session", &cmd1).await.map_err(|_| ProvError::WrongCode)?;
        let resp1 = Fields::parse(&resp1).map_err(link)?;
        let sr1 = resp1.msg(12).map_err(link)?.msg(23).map_err(link)?;
        if sr1.int(1) != 0 {
            return Err(ProvError::WrongCode);
        }
        /* The device proves it knows the code too (so it's not an
         * impostor that just accepts anything). */
        if !same(&sr1.bytes(2), &challenge.expected_device_proof) {
            return Err(link("the device's proof is wrong: not the device whose code this is"));
        }
        let nonce: [u8; 12] = sr1.bytes(3).try_into().map_err(|_| link("the device sent a bad nonce"))?;
        let key = UnboundKey::new(&AES_256_GCM, &challenge.key[..32]).map_err(|_| link("AES key"))?;
        Ok(Session {
            transport,
            key: LessSafeKey::new(key),
            nonce,
            counting,
        })
    }

    fn next_nonce(&mut self) -> Nonce {
        let nonce = Nonce::assume_unique_for_key(self.nonce);
        if self.counting {
            let counter = u32::from_be_bytes(self.nonce[8..].try_into().unwrap()).wrapping_add(1);
            self.nonce[8..].copy_from_slice(&counter.to_be_bytes());
        }
        nonce
    }

    /* One encrypted prov-config request -> the decrypted answer. */
    async fn config(&mut self, request: Vec<u8>) -> Result<Fields, ProvError> {
        let mut sealed = request;
        let nonce = self.next_nonce();
        self.key
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut sealed)
            .map_err(|_| link("encrypting"))?;
        let mut answer = self.transport.exchange("prov-config", &sealed).await.map_err(link)?;
        let nonce = self.next_nonce();
        let plain = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut answer)
            .map_err(|_| link("the device's answer failed its integrity check"))?;
        Fields::parse(plain).map_err(link)
    }

    /* Sends the WiFi name and password. */
    pub async fn set_wifi(&mut self, ssid: &str, password: &str) -> Result<(), ProvError> {
        let request = Msg::new()
            .int(1, 2)
            .msg(12, Msg::new().bytes(1, ssid.as_bytes()).bytes(2, password.as_bytes()))
            .build();
        let answer = self.config(request).await?;
        if answer.int(1) != 3 || answer.msg(13).map_err(link)?.int(1) != 0 {
            return Err(link("the device refused the WiFi details"));
        }
        Ok(())
    }

    /* Tells the device to join. */
    pub async fn apply(&mut self) -> Result<(), ProvError> {
        let answer = self.config(Msg::new().int(1, 4).msg(14, Msg::new()).build()).await?;
        if answer.int(1) != 5 || answer.msg(15).map_err(link)?.int(1) != 0 {
            return Err(link("the device refused to join"));
        }
        Ok(())
    }

    /* Asks how joining goes. */
    pub async fn status(&mut self) -> Result<WifiState, ProvError> {
        let answer = self.config(Msg::new().int(1, 0).msg(10, Msg::new()).build()).await?;
        let status = answer.msg(11).map_err(link)?;
        if answer.int(1) != 1 || status.int(1) != 0 {
            return Err(link("the device couldn't tell its WiFi status"));
        }
        /* wifi_sta_state: 0 Connected (so: also when missing), 1
         * Connecting, 2 Disconnected, 3 ConnectionFailed. */
        Ok(match status.int(2) {
            0 => {
                let connected = status.msg(11).map_err(link)?;
                WifiState::Connected {
                    ip: String::from_utf8_lossy(&connected.bytes(1)).into_owned(),
                }
            }
            3 => WifiState::Failed(match status.int(10) {
                0 => ProvError::WifiPassword,
                _ => ProvError::WifiNotFound,
            }),
            _ => WifiState::Connecting,
        })
    }
}

/* The whole setup over one transport: session, WiFi details, join, wait.
 * Returns the device's IP address on the WiFi. */
pub async fn provision<T: Transport>(transport: T, code: &str, ssid: &str, password: &str, timing: Timing) -> Result<String, ProvError> {
    let mut session = Session::open(transport, code).await?;
    session.set_wifi(ssid, password).await?;
    session.apply().await?;
    let deadline = tokio::time::Instant::now() + timing.join_timeout;
    loop {
        tokio::time::sleep(timing.poll).await;
        match session.status().await? {
            WifiState::Connected { ip } => return Ok(ip),
            WifiState::Failed(why) => return Err(why),
            WifiState::Connecting if tokio::time::Instant::now() > deadline => return Err(ProvError::Timeout),
            WifiState::Connecting => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::srp::device::{verifier, Device};
    use super::*;

    /* A simulated ESP32 running Espressif's provisioning, as far as the
     * hub can see it: the same messages, the same SRP6a (srp::device),
     * the same AES-GCM with a counting nonce. */
    struct FakeDevice {
        salt: Vec<u8>,
        srp: Device,
        client_public: Vec<u8>,
        key: Option<LessSafeKey>,
        nonce: [u8; 12],
        /* What "status?" answers: Connecting this many times, then this. */
        connecting_for: u32,
        outcome: WifiState,
        ssid: String,
    }

    impl FakeDevice {
        fn new(code: &str, outcome: WifiState) -> FakeDevice {
            let salt = vec![0x5a; 16];
            FakeDevice {
                srp: Device::new(verifier(USERNAME, code, &salt), &[0x61; 32]),
                salt,
                client_public: Vec::new(),
                key: None,
                nonce: [7; 12],
                connecting_for: 2,
                outcome,
                ssid: String::new(),
            }
        }

        fn step_nonce(&mut self) -> Nonce {
            let nonce = Nonce::assume_unique_for_key(self.nonce);
            let counter = u32::from_be_bytes(self.nonce[8..].try_into().unwrap()).wrapping_add(1);
            self.nonce[8..].copy_from_slice(&counter.to_be_bytes());
            nonce
        }

        fn handle(&mut self, endpoint: &str, data: &[u8]) -> Result<Vec<u8>, String> {
            match endpoint {
                "proto-ver" => Ok(br#"{"prov":{"ver":"v1.1","sec_ver":2,"sec_patch_ver":1,"cap":["wifi_scan"]}}"#.to_vec()),
                "prov-session" => {
                    let sec2 = Fields::parse(data)?.msg(12)?;
                    match sec2.int(1) {
                        0 => {
                            self.client_public = sec2.msg(20)?.bytes(2);
                            Ok(Msg::new()
                                .int(2, 2)
                                .msg(12, Msg::new().int(1, 1).msg(21, Msg::new().bytes(2, &self.srp.public).bytes(3, &self.salt)))
                                .build())
                        }
                        2 => {
                            let proof = sec2.msg(22)?.bytes(1);
                            let (device_proof, key) = self
                                .srp
                                .verify(USERNAME, &self.salt, &self.client_public, &proof)
                                /* the real device drops the connection */
                                .ok_or("GATT Protocol Error: Invalid PDU")?;
                            self.key = Some(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key[..32]).unwrap()));
                            Ok(Msg::new()
                                .int(2, 2)
                                .msg(12, Msg::new().int(1, 3).msg(23, Msg::new().bytes(2, &device_proof).bytes(3, &self.nonce)))
                                .build())
                        }
                        _ => Err("unexpected session message".into()),
                    }
                }
                "prov-config" => {
                    let mut request = data.to_vec();
                    let nonce = self.step_nonce();
                    let key = self.key.as_ref().ok_or("no session")?;
                    let plain = key.open_in_place(nonce, Aad::empty(), &mut request).map_err(|_| "bad tag")?;
                    let request = Fields::parse(plain)?;
                    let answer = match request.int(1) {
                        2 => {
                            self.ssid = String::from_utf8_lossy(&request.msg(12)?.bytes(1)).into_owned();
                            Msg::new().int(1, 3).msg(13, Msg::new())
                        }
                        4 => Msg::new().int(1, 5).msg(15, Msg::new()),
                        0 => {
                            let status = if self.connecting_for > 0 {
                                self.connecting_for -= 1;
                                Msg::new().int(2, 1)
                            } else {
                                match &self.outcome {
                                    WifiState::Connected { ip } => Msg::new().int(2, 0).msg(11, Msg::new().bytes(1, ip.as_bytes())),
                                    WifiState::Failed(ProvError::WifiPassword) => Msg::new().int(2, 3).int(10, 0),
                                    _ => Msg::new().int(2, 3).int(10, 1),
                                }
                            };
                            Msg::new().int(1, 1).msg(11, status)
                        }
                        _ => return Err("unexpected config message".into()),
                    };
                    let mut sealed = answer.build();
                    let nonce = self.step_nonce();
                    self.key.as_ref().unwrap().seal_in_place_append_tag(nonce, Aad::empty(), &mut sealed).unwrap();
                    Ok(sealed)
                }
                other => Err(format!("no endpoint {other}")),
            }
        }
    }

    impl Transport for FakeDevice {
        fn exchange<'a>(&'a mut self, endpoint: &'a str, data: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move { self.handle(endpoint, data) })
        }
    }

    const CODE: &str = "5MCNWM1904CHQ2GT";
    const FAST: Timing = Timing {
        poll: Duration::from_millis(1),
        join_timeout: Duration::from_millis(50),
    };

    #[tokio::test]
    async fn sets_up_the_wifi_and_returns_the_address() {
        let device = FakeDevice::new(CODE, WifiState::Connected { ip: "192.168.1.143".into() });
        let ip = provision(device, CODE, "DIGI-F9rT", "secret123", FAST).await.unwrap();
        assert_eq!(ip, "192.168.1.143");
    }

    #[tokio::test]
    async fn a_wrong_code_is_reported_as_such() {
        let device = FakeDevice::new(CODE, WifiState::Connected { ip: "x".into() });
        assert_eq!(provision(device, "AAAABBBBCCCCDDDD", "n", "p", FAST).await, Err(ProvError::WrongCode));
    }

    #[tokio::test]
    async fn wifi_failures_are_told_apart() {
        let device = FakeDevice::new(CODE, WifiState::Failed(ProvError::WifiPassword));
        assert_eq!(provision(device, CODE, "n", "wrong", FAST).await, Err(ProvError::WifiPassword));
        let device = FakeDevice::new(CODE, WifiState::Failed(ProvError::WifiNotFound));
        assert_eq!(provision(device, CODE, "5G-only", "p", FAST).await, Err(ProvError::WifiNotFound));
    }

    #[tokio::test]
    async fn a_device_that_never_joins_times_out() {
        let mut device = FakeDevice::new(CODE, WifiState::Connecting);
        device.connecting_for = u32::MAX;
        assert_eq!(provision(device, CODE, "n", "p", FAST).await, Err(ProvError::Timeout));
    }
}
