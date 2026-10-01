/*
 * srp.rs -- the client half of SRP6a as Espressif's provisioning uses it
 * ("security 2", issue #42): the hub proves it knows a device's pairing
 * code, and both sides end up with the same session key, without the code
 * ever being sent.
 *
 * SRP6a in one paragraph: the device keeps a "verifier" v = g^x mod N made
 * from the code (x = a hash of salt, user name and code). Each side sends a
 * random public value (A from us, B from the device). Both compute the same
 * secret S -- we from the code, the device from v -- and prove it to each
 * other with hashes (M from us, H(A, M, K) from the device). Without the
 * code, nobody can compute S; someone listening learns nothing that lets
 * them test codes offline.
 *
 * WHY THIS FILE IS SO LITERAL. Every byte that goes into a hash must be the
 * same on both sides, and Espressif's choices are specific: SHA-512, the
 * 3072-bit group of RFC 5054 (g = 5), some values padded to the group's
 * length before hashing (k, u) and others with their leading zero bytes
 * stripped (s, A, B, S, the inner hash in x). This follows their reference
 * client exactly (network_provisioning: tool/esp_prov/security/srp6a.py),
 * formula by formula; the tests below include a device-side emulation
 * checking that both halves agree.
 */
use num_bigint::BigUint;
use ring::digest::{Context, SHA512};

/* RFC 5054's 3072-bit prime and its generator 5. */
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;

/* N's length in bytes: what "padded" means below. */
const N_LEN: usize = 384;

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("the RFC 5054 prime is valid hex")
}

/* SHA-512 over several parts, one after the other. */
fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = Context::new(&SHA512);
    for part in parts {
        ctx.update(part);
    }
    ctx.finish().as_ref().to_vec()
}

/* The number as bytes WITHOUT leading zeros (Python's long_to_bytes;
 * zero is one zero byte). */
fn stripped(n: &BigUint) -> Vec<u8> {
    n.to_bytes_be()
}

/* The number as exactly N_LEN bytes, zeros in front. */
fn padded(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; N_LEN.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

/* The multiplier k = H(N, pad(g)). */
fn k() -> BigUint {
    BigUint::from_bytes_be(&sha512(&[&padded(&n()), &padded(&BigUint::from(G))]))
}

/* x = H(s, H(user ":" password)) -- note the inner hash goes in as a
 * NUMBER, i.e. with its leading zero bytes stripped. */
fn x(salt: &BigUint, username: &str, password: &str) -> BigUint {
    let inner = BigUint::from_bytes_be(&sha512(&[format!("{username}:{password}").as_bytes()]));
    BigUint::from_bytes_be(&sha512(&[&stripped(salt), &stripped(&inner)]))
}

/* H(N) XOR H(pad(g)): part of M. */
fn h_n_xor_g() -> Vec<u8> {
    let h_n = sha512(&[&stripped(&n())]);
    let h_g = sha512(&[&padded(&BigUint::from(G))]);
    h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect()
}

/* What the device must prove, and the shared key, once our proof M is
 * sent. */
pub struct Challenge {
    /* Our proof M, sent to the device. */
    pub proof: Vec<u8>,
    /* H(A, M, K): the device's proof must equal this. */
    pub expected_device_proof: Vec<u8>,
    /* K = H(S), 64 bytes: the session key (AES uses the first 32). */
    pub key: Vec<u8>,
}

pub struct Client {
    username: String,
    password: String,
    a: BigUint,
    /* Our public value A, as sent: always exactly N_LEN bytes. */
    pub public: Vec<u8>,
}

impl Client {
    /* A fresh client. `random` fills a buffer with secure random bytes
     * (ring's SystemRandom in real use; fixed bytes in tests). */
    pub fn new(username: &str, password: &str, mut random: impl FnMut(&mut [u8])) -> Client {
        let (n, g) = (n(), BigUint::from(G));
        /* A random 256-bit a with its top bit set. The device keeps A as
         * received and hashes it again later; a value with a leading zero
         * byte (1 in 256) would hash differently on the two sides, so it's
         * re-rolled until A fills all N_LEN bytes (as the reference does). */
        for _ in 0..10_000 {
            let mut bytes = [0u8; 32];
            random(&mut bytes);
            bytes[0] |= 0x80;
            let a = BigUint::from_bytes_be(&bytes);
            let public = g.modpow(&a, &n);
            if public.to_bytes_be().len() == N_LEN {
                return Client {
                    username: username.to_string(),
                    password: password.to_string(),
                    a,
                    public: public.to_bytes_be(),
                };
            }
        }
        unreachable!("10,000 random values in a row with a leading zero byte");
    }

    /* The device's salt and public value B -> our proof, and what to
     * expect back. None if B is invalid (the SRP6a safety checks). */
    pub fn challenge(&self, salt: &[u8], device_public: &[u8]) -> Option<Challenge> {
        let n = n();
        let g = BigUint::from(G);
        let a_pub = BigUint::from_bytes_be(&self.public);
        let b_pub = BigUint::from_bytes_be(device_public);
        let zero = BigUint::from(0u32);
        if &b_pub % &n == zero {
            return None;
        }
        let u = BigUint::from_bytes_be(&sha512(&[&padded(&a_pub), &padded(&b_pub)]));
        if u == zero {
            return None;
        }
        let s = BigUint::from_bytes_be(salt);
        let x = x(&s, &self.username, &self.password);
        let v = g.modpow(&x, &n);
        /* S = (B - k*v)^(a + u*x) mod N, the base kept positive. */
        let kv = (k() * &v) % &n;
        let base = ((&b_pub % &n) + &n - kv) % &n;
        let shared = base.modpow(&(&self.a + &u * &x), &n);
        let key = sha512(&[&stripped(&shared)]);
        let proof = sha512(&[
            &h_n_xor_g(),
            &sha512(&[self.username.as_bytes()]),
            &stripped(&s),
            &stripped(&a_pub),
            &stripped(&b_pub),
            &key,
        ]);
        let expected_device_proof = sha512(&[&stripped(&a_pub), &proof, &key]);
        Some(Challenge {
            proof,
            expected_device_proof,
            key,
        })
    }
}

/* ---- the device's half, for tests: what esp_srp.c does ---- */
#[cfg(test)]
pub mod device {
    use super::*;

    /* The verifier the device makes from the code (esp_srp_gen_salt_verifier). */
    pub fn verifier(username: &str, password: &str, salt: &[u8]) -> BigUint {
        BigUint::from(G).modpow(&x(&BigUint::from_bytes_be(salt), username, password), &n())
    }

    /* B = k*v + g^b, and (after receiving A and M) whether M is right and
     * the device's proof. */
    pub struct Device {
        pub b: BigUint,
        pub public: Vec<u8>,
        v: BigUint,
    }

    impl Device {
        pub fn new(v: BigUint, b_bytes: &[u8]) -> Device {
            let n = n();
            let b = BigUint::from_bytes_be(b_bytes);
            let public = ((k() * &v) + BigUint::from(G).modpow(&b, &n)) % &n;
            Device {
                b,
                public: public.to_bytes_be(),
                v,
            }
        }

        /* Checks the client's proof; returns the device's proof and K. */
        pub fn verify(&self, username: &str, salt: &[u8], a_pub: &[u8], client_proof: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
            let n = n();
            let a = BigUint::from_bytes_be(a_pub);
            let b_pub = BigUint::from_bytes_be(&self.public);
            let u = BigUint::from_bytes_be(&sha512(&[&padded(&a), &padded(&b_pub)]));
            /* S = (A * v^u)^b mod N */
            let shared = ((&a * self.v.modpow(&u, &n)) % &n).modpow(&self.b, &n);
            let key = sha512(&[&stripped(&shared)]);
            let s = BigUint::from_bytes_be(salt);
            let expected = sha512(&[
                &h_n_xor_g(),
                &sha512(&[username.as_bytes()]),
                &stripped(&s),
                &stripped(&a),
                &stripped(&b_pub),
                &key,
            ]);
            if expected != client_proof {
                return None;
            }
            Some((sha512(&[&stripped(&a), client_proof, &key]), key))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::device::{verifier, Device};
    use super::*;

    /* A counter as "random" bytes: tests repeat exactly. */
    fn counting(seed: u8) -> impl FnMut(&mut [u8]) {
        let mut next = seed;
        move |buf: &mut [u8]| {
            for byte in buf.iter_mut() {
                *byte = next;
                next = next.wrapping_mul(31).wrapping_add(7);
            }
        }
    }

    /* Computed with Espressif's own client (tool/esp_prov/security/
     * srp6a.py) from the same fixed inputs: a = 9f 01 02 .. 1f, this salt
     * (with a leading zero byte on purpose), code 5MCNWM1904CHQ2GT, and a
     * device value B made from b = 44 44 .. 44. Byte for byte the same, so
     * the real device, which accepts that client, accepts this one. */
    #[test]
    fn matches_espressifs_reference_client() {
        let a_bytes: Vec<u8> = std::iter::once(0x9fu8).chain(1..32u8).collect();
        let client = Client::new("wifiprov", "5MCNWM1904CHQ2GT", |buf: &mut [u8]| buf.copy_from_slice(&a_bytes));
        let hex = |parts: &[&str]| -> Vec<u8> {
            let text: String = parts.concat();
            (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
        };
        let salt = hex(&["0013a5a5a5a5a5a5a5a5a5a5a5a5a5a5"]);
        assert_eq!(client.public, hex(&[
            "41b7bd84a7a43c4816d0f400492f6610e1c69b468cd662a325977eb9594b5e83",
            "460b2b12516b73844c67c03f46831026f1a7ff0d671d8f253c9fffae76def19a",
            "89fa9377ce6c50296e6c6c2a787346d6590f796b824ea4b022e3b63dbd295f8a",
            "2926d0ddafffb5f1c46a3a8072d1c39c7391b9a9fcdc58278b59c5c2dfcc9e43",
            "cf66bfa5dfa5575cda92749ad3f6957902172e16f25b578b57e6eef448d107f8",
            "6f977b11394ca21cb18418462a5a43fc2161ae1944321c95283650cb5c70906a",
            "b340416bfbdea0e6fc6a9549fdabc7fbd31514d4791c89affe40304b1f43421a",
            "5796eedac6afe9b3bcac2e3993366b40423d52d887a627f6435d71fd9352c807",
            "b444a9ce2865ae5a41c52761683804ae3a9c415f1c6260518920d3a4e478f19e",
            "46fcc5470329dc704889f6e74cb8d6a1b39bd240c37a486a1fad1efa30c9e276",
            "5345e85da6f5699e1933b8399512b1e638832d527958b618e478ba7624e31272",
            "8564ab9cd65e6d495625d8576d1fad285fe2a3d4961b9c1483fbbbe5e977c354",
        ]));
        let b_pub = hex(&[
            "7e46aaa9d8574f2bb91ec52b0c1dac347dd799a89236e2ac0c4427b717afa429",
            "854bbe1cea320682001798f12018273ae941dc0a68e06b17d6f6d89e6d0310e7",
            "1db2548ea1e884786b1df7f653665c3deda1f9ed1d53d4e4c8f54032cf1b8ed0",
            "c646a1d073015f1e7d34350ead48df2cd5b79e7b1df99042702d0d021d882534",
            "6ad6da9681aa168839180d2d1ca670d87bbb767a5e06d366c3c19bec6fe6dd1e",
            "17df6041d7540afcdc007e1b771201b22581183229726e2de4dea28c2917b9de",
            "40e2c2eb22c942c503857d26a7f6650e0d06bc47a403da69dce51f66e8c98406",
            "79d7d714ccc6b1eab99872b603970c38c45cb92db6f3598838c5c5bca02566eb",
            "98a5ccc3a5237c50d7d0d1f211788e668354f331f784ca2003312527f83d149c",
            "529cdd9f3a2fb2069dd442e4fd651507dc74cec216bd99c47bd456c5edd55e12",
            "0a2b4a2136c2c1e46c4975ee6820f7d06589d3c1eabbdbc7dbd88f165559c987",
            "7e42deba64554a77c4317cafdf6039d5e8f3a3564521f65c787c0c1b455de14a",
        ]);
        let c = client.challenge(&salt, &b_pub).unwrap();
        assert_eq!(c.proof, hex(&[
            "5fcd897a054f8d86bf67221aca1ecba25c54b44eaff6fb908f8d42022b9cf5b6",
            "2b15b1ccda80bb8db0964380135f7e40029e17bbe56f19936237ce47d0df7876",
        ]));
        assert_eq!(c.key, hex(&[
            "2bbe093911324bee1e19bb884a47f1e96dae0efddf457ffffcf669e6d7891db9",
            "f48f3c1b189c376bc1ceec283d0a53a321dfb0c7bf6d279702b6161f87901930",
        ]));
        assert_eq!(c.expected_device_proof, hex(&[
            "506aa1680c585be4fecec50ed11b152afad47afe6f993c06d85c703f4df72172",
            "20878f328cd4db406131eb83dd4604221f4a8db1b330b6e50a56b47181d83047",
        ]));
    }

    #[test]
    fn both_halves_agree_with_the_right_code() {
        let salt = [0x42u8; 16];
        let device = Device::new(verifier("wifiprov", "5MCNWM1904CHQ2GT", &salt), &[0x33; 32]);
        let client = Client::new("wifiprov", "5MCNWM1904CHQ2GT", counting(1));
        assert_eq!(client.public.len(), N_LEN);
        let challenge = client.challenge(&salt, &device.public).unwrap();
        let (device_proof, device_key) = device
            .verify("wifiprov", &salt, &client.public, &challenge.proof)
            .expect("the device accepts our proof");
        assert_eq!(device_proof, challenge.expected_device_proof);
        assert_eq!(device_key, challenge.key);
        assert_eq!(challenge.key.len(), 64);
    }

    #[test]
    fn a_wrong_code_is_refused_by_the_device() {
        let salt = [0x07u8; 16];
        let device = Device::new(verifier("wifiprov", "5MCNWM1904CHQ2GT", &salt), &[0x55; 32]);
        let client = Client::new("wifiprov", "AAAABBBBCCCCDDDD", counting(9));
        let challenge = client.challenge(&salt, &device.public).unwrap();
        assert!(device.verify("wifiprov", &salt, &client.public, &challenge.proof).is_none());
    }

    #[test]
    fn a_zero_device_value_is_refused() {
        let client = Client::new("wifiprov", "x", counting(3));
        assert!(client.challenge(&[1], &padded(&n())).is_none()); /* B = N = 0 mod N */
    }

    #[test]
    fn a_salt_with_a_leading_zero_still_agrees() {
        /* The stripping rules matter exactly here. */
        let salt = [0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        let device = Device::new(verifier("wifiprov", "CODE", &salt), &[0x21; 32]);
        let client = Client::new("wifiprov", "CODE", counting(5));
        let challenge = client.challenge(&salt, &device.public).unwrap();
        assert!(device.verify("wifiprov", &salt, &client.public, &challenge.proof).is_some());
    }
}
