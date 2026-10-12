/*
 * roborock_cloud.rs -- the Roborock account, used ONCE at setup (issue
 * #74, pattern P4): log in with a code sent by email, read the account's
 * vacuums and their `local_key`s. After that the hub talks to the vacuum
 * on the LAN only (roborock_proto.rs), and nothing here runs again --
 * until the vacuum is reset and paired with the app anew (new key: the
 * device shows "unauthorized", and "Pair again" comes back here).
 *
 * The hub never sees the account PASSWORD: Roborock offers a login with a
 * one-time code by email, and that's the one used. The session it gives
 * is only kept for the minutes of the wizard (in the wizard's secret
 * values), never stored.
 *
 * Unofficial API, as the Roborock app uses it; followed from
 * python-roborock (web_api.py). The steps:
 *   1. getUrlByEmail     which of Roborock's regional servers has this
 *                        account (EU, US, CN, RU), and its country
 *   2. email/code/send   Roborock emails a 6-digit code
 *   3. key/sign + agreement/latest + auth/email/login/code
 *                        the code -> a session: a `token` and "rriot"
 *                        keys for signing further requests
 *   4. getHomeDetail + /v3/user/homes/<id>
 *                        the account's devices, with their local keys
 * Every request carries a `header_clientid`: MD5(email + an id made up for
 * this login), base64. Roborock ties the emailed code to it, so steps 2
 * and 3 must use the same one -- kept in the wizard's values between the
 * two (`login_client`).
 */
use base64::Engine;
use hyper::Method;
use md5::{Digest, Md5};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use super::cloud::{https_json, CloudRequest};
use super::SetupError;
use crate::templates::ErrorKind;

/* Tried in turn until one knows the email. */
const BASE_URLS: [&str; 4] = [
    "https://euiot.roborock.com",
    "https://usiot.roborock.com",
    "https://cniot.roborock.com",
    "https://ruiot.roborock.com",
];

/* What the Roborock app says it is: the login is refused for unknown apps. */
const APP_HEADERS: [(&str, &str); 4] = [
    ("header_clientlang", "en"),
    ("header_appversion", "4.54.02"),
    ("header_phonesystem", "iOS"),
    ("header_phonemodel", "iPhone16,1"),
];

/* Between "send the code" and "log in" (plain values, not secret: none of
 * them lets anyone in without the code). */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Pending {
    pub base_url: String,
    pub country: String,
    pub country_code: String,
    /* header_clientid, see the top of the file. */
    pub client: String,
}

/* A logged-in session: the token and the signing keys ("rriot"). */
#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
    pub base_url: String,
    pub client: String,
    pub token: String,
    /* rriot.u, .s, .h, .k and the API server rriot.r.a. */
    pub u: String,
    pub s: String,
    pub h: String,
    pub k: String,
    pub api: String,
    /* Roborock's MQTT broker ("ssl://mqtt-eu-3.roborock.com:8883"): maps
     * only come through it (roborock_map.rs). */
    #[serde(default)]
    pub mqtt: String,
    /* When the hub logged in (Unix seconds; 0: a session from before this
     * was kept). Only for the logs: how long Roborock's sessions last. */
    #[serde(default)]
    pub logged_in: u64,
}

impl Session {
    /* A short fingerprint for the logs: tells sessions apart without
     * showing any of their keys. */
    pub fn fingerprint(&self) -> String {
        Md5::digest(format!("{}:{}", self.token, self.k).as_bytes())[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    /* "logged in 2026-10-04 18:02 UTC (41 h ago)" or "login time unknown". */
    pub fn age(&self) -> String {
        if self.logged_in == 0 {
            return "login time unknown".into();
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        format!("logged in {} h ago", now.saturating_sub(self.logged_in) / 3600)
    }
}

/* One device of the account. */
#[derive(Debug, Clone, PartialEq)]
pub struct CloudVacuum {
    pub duid: String,
    pub name: String,
    pub model: String,
    pub local_key: String,
    /* The LAN protocol: "1.0" works; "A01"/"B01"/"L01" (newer models) not
     * yet. */
    pub protocol: String,
    pub online: bool,
}

/* One request to Roborock's cloud. Not reaching it is the cloud's
 * problem (or the hub's internet), not the vacuum's: said so. */
async fn call(method: Method, url: &str, request: CloudRequest<'_>) -> Result<Value, SetupError> {
    https_json(method, url, request)
        .await
        .map_err(|e| SetupError::new(ErrorKind::Vendor, format!("Can't reach Roborock's servers ({e}). Is the hub online?")))
}

fn setup_error(kind: ErrorKind, text: impl Into<String>) -> SetupError {
    SetupError::new(kind, text)
}

fn random_text(len: usize) -> String {
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0u8; len];
    SystemRandom::new().fill(&mut bytes).expect("the system's random source");
    bytes.iter().map(|b| LETTERS[*b as usize % LETTERS.len()] as char).collect()
}

/* "code" and "msg" of a Roborock reply; Ok(data) for code 200. */
fn checked(reply: Value, what: &str) -> Result<Value, (i64, String)> {
    match reply["code"].as_i64() {
        Some(200) => Ok(reply["data"].clone()),
        code => Err((code.unwrap_or(-1), format!("{what}: {} (code {})", reply["msg"].as_str().unwrap_or("?"), code.unwrap_or(-1)))),
    }
}

/* Steps 1 and 2: finds the account's server, has the code emailed. */
pub async fn send_code(email: &str) -> Result<Pending, SetupError> {
    let mut found = None;
    for base in BASE_URLS {
        let query = [("email", email.to_string()), ("needtwostepauth", "false".to_string())];
        let Ok(reply) = call(Method::POST, &format!("{base}/api/v1/getUrlByEmail"), CloudRequest { query: &query, ..Default::default() }).await
        else {
            continue;
        };
        match checked(reply, "finding the account") {
            Ok(data) if data["url"].is_string() && (data["country"].is_string() || !data["countrycode"].is_null()) => {
                found = Some(data);
                break;
            }
            Ok(_) => continue,
            Err((2003, _)) => return Err(setup_error(ErrorKind::Vendor, "That isn't an email address Roborock accepts.")),
            Err(_) => continue,
        }
    }
    let data = found.ok_or_else(|| {
        setup_error(ErrorKind::Vendor, "Roborock doesn't know this email. Use the one you log in to the Roborock app with.")
    })?;
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    let device_id = random_text(22);
    let pending = Pending {
        base_url: text(&data["url"]),
        country: text(&data["country"]),
        country_code: text(&data["countrycode"]),
        client: base64::engine::general_purpose::STANDARD.encode(Md5::new().chain_update(email).chain_update(&device_id).finalize()),
    };
    let form = [("email", email.to_string()), ("type", "login".to_string()), ("platform", String::new())];
    let headers = [("header_clientid", pending.client.clone()), ("header_clientlang", "en".to_string())];
    let reply = call(
        Method::POST,
        &format!("{}/api/v4/email/code/send", pending.base_url),
        CloudRequest { form: Some(&form), headers: &headers, ..Default::default() },
    )
    .await?;
    match checked(reply, "sending the code") {
        Ok(_) => Ok(pending),
        Err((2008, _)) => Err(setup_error(ErrorKind::Vendor, "Roborock doesn't know this email. Use the one you log in to the Roborock app with.")),
        Err((9002, _)) => Err(setup_error(ErrorKind::Vendor, "Roborock sent too many codes to this email. Wait a while, then try again.")),
        Err((_, detail)) => Err(setup_error(ErrorKind::Vendor, detail)),
    }
}

/* Step 3: the emailed code -> a session. */
pub async fn login(email: &str, code: &str, pending: &Pending) -> Result<Session, SetupError> {
    let base = &pending.base_url;
    let client = [("header_clientid", pending.client.clone())];
    /* The login wants a request signature from Roborock's own signing
     * service ("x-mercy-k" for a random "x-mercy-ks"). */
    let ks = random_text(16);
    let signed = call(
        Method::POST,
        &format!("{base}/api/v3/key/sign"),
        CloudRequest { query: &[("s", ks.clone())], headers: &client, ..Default::default() },
    )
    .await?;
    let k = checked(signed, "signing the login")
        .map_err(|(_, d)| setup_error(ErrorKind::Vendor, d))?["k"]
        .as_str()
        .ok_or_else(|| setup_error(ErrorKind::Vendor, "Roborock's login changed (no signature)"))?
        .to_string();
    /* The terms of use version the account accepted in the app; a stale
     * one is refused. Fall back to the last known. */
    let (major, minor) = match call(
        Method::GET,
        &format!("{base}/api/v3/app/agreement/latest"),
        CloudRequest { query: &[("country", pending.country.clone())], headers: &[("header_clientlang", "en".into())], ..Default::default() },
    )
    .await
    .ok()
    .and_then(|r| checked(r, "agreement").ok())
    {
        Some(d) if d["majorVersion"].is_i64() => (d["majorVersion"].to_string(), d["minorVersion"].as_i64().unwrap_or(0).to_string()),
        _ => ("14".to_string(), "0".to_string()),
    };
    let form = [
        ("country", pending.country.clone()),
        ("countryCode", pending.country_code.clone()),
        ("email", email.to_string()),
        ("code", code.to_string()),
        ("majorVersion", major),
        ("minorVersion", minor),
    ];
    let mut headers: Vec<(&str, String)> = vec![
        ("header_clientid", pending.client.clone()),
        ("x-mercy-ks", ks),
        ("x-mercy-k", k),
    ];
    headers.extend(APP_HEADERS.iter().map(|(n, v)| (*n, v.to_string())));
    let reply = call(
        Method::POST,
        &format!("{base}/api/v4/auth/email/login/code"),
        CloudRequest { form: Some(&form), headers: &headers, ..Default::default() },
    )
    .await?;
    let data = match checked(reply, "logging in") {
        Ok(data) => data,
        Err((2018, _)) => return Err(setup_error(ErrorKind::Vendor, "That code isn't right (or it expired). Check the email, or ask for a new code.")),
        Err((3009, _)) | Err((3006, _)) => {
            return Err(setup_error(ErrorKind::Vendor, "Open the Roborock app once and accept its terms of use, then try again."))
        }
        Err((3039, _)) => return Err(setup_error(ErrorKind::Vendor, "Roborock doesn't know this account on this server.")),
        Err((_, detail)) => return Err(setup_error(ErrorKind::Vendor, detail)),
    };
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    let rriot = &data["rriot"];
    let session = Session {
        base_url: base.clone(),
        client: pending.client.clone(),
        token: text(&data["token"]),
        u: text(&rriot["u"]),
        s: text(&rriot["s"]),
        h: text(&rriot["h"]),
        k: text(&rriot["k"]),
        api: text(&rriot["r"]["a"]),
        mqtt: text(&rriot["r"]["m"]),
        logged_in: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    if session.token.is_empty() || session.u.is_empty() || session.h.is_empty() || !session.api.starts_with("https://") {
        return Err(setup_error(ErrorKind::Vendor, "Roborock's login answer changed (no session)"));
    }
    Ok(session)
}

/* The "Hawk" signature Roborock's device API wants on every request:
 * HMAC-SHA256 with the session's `h` over "u:s:nonce:time:MD5(path)::". */
fn hawk(session: &Session, path: &str, timestamp: u64, nonce: &str) -> String {
    let path_md5 = hex(&Md5::digest(path.as_bytes()));
    let text = format!("{}:{}:{nonce}:{timestamp}:{path_md5}::", session.u, session.s);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, session.h.as_bytes());
    let mac = base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&key, text.as_bytes()).as_ref());
    format!("Hawk id=\"{}\",s=\"{}\",ts=\"{timestamp}\",nonce=\"{nonce}\",mac=\"{mac}\"", session.u, session.s)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/* Step 4: the account's vacuums. */
pub async fn devices(session: &Session) -> Result<Vec<CloudVacuum>, SetupError> {
    let headers = [("header_clientid", session.client.clone()), ("Authorization", session.token.clone())];
    let reply = call(
        Method::GET,
        &format!("{}/api/v1/getHomeDetail", session.base_url),
        CloudRequest { headers: &headers, ..Default::default() },
    )
    .await?;
    let home = match checked(reply, "reading the home") {
        Ok(data) => data["rrHomeId"].clone(),
        Err((2010, _)) => return Err(setup_error(ErrorKind::Vendor, "Roborock ended the session. Log in again.")),
        Err((_, detail)) => return Err(setup_error(ErrorKind::Vendor, detail)),
    };
    let home = home.as_i64().map(|h| h.to_string()).or_else(|| home.as_str().map(str::to_string)).ok_or_else(|| {
        setup_error(ErrorKind::Vendor, "This Roborock account has no home yet: add the vacuum in the Roborock app first.")
    })?;
    let mut last_error = None;
    for path in [format!("/v3/user/homes/{home}"), format!("/user/homes/{home}")] {
        let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let auth = hawk(session, &path, timestamp, &random_text(8));
        match call(Method::GET, &format!("{}{path}", session.api.trim_end_matches('/')), CloudRequest {
            headers: &[("Authorization", auth)],
            ..Default::default()
        })
        .await
        {
            Ok(reply) if reply["success"].as_bool() == Some(true) => return Ok(parse_home(&reply["result"])),
            Ok(reply) => last_error = Some(format!("reading the devices: {}", reply["msg"].as_str().unwrap_or("refused"))),
            Err(e) => last_error = Some(e.detail),
        }
    }
    Err(setup_error(ErrorKind::Vendor, last_error.unwrap_or_default()))
}

/* The home's rooms as Roborock's cloud names them: [{"id": 123,
 * "name": "Kitchen"}, ...]. A vacuum's own room numbers map to these ids
 * (get_room_mapping). */
pub async fn rooms(session: &Session) -> Result<Value, SetupError> {
    let headers = [("header_clientid", session.client.clone()), ("Authorization", session.token.clone())];
    let reply = call(Method::GET, &format!("{}/api/v1/getHomeDetail", session.base_url), CloudRequest { headers: &headers, ..Default::default() }).await?;
    let home = checked(reply, "reading the home").map_err(|(_, d)| setup_error(ErrorKind::Vendor, d))?["rrHomeId"].to_string();
    let path = format!("/user/homes/{home}/rooms");
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let auth = hawk(session, &path, timestamp, &random_text(8));
    let reply = call(Method::GET, &format!("{}{path}", session.api.trim_end_matches('/')), CloudRequest { headers: &[("Authorization", auth)], ..Default::default() }).await?;
    if reply["success"].as_bool() != Some(true) {
        return Err(setup_error(ErrorKind::Vendor, format!("reading the rooms: {}", reply["msg"].as_str().unwrap_or("refused"))));
    }
    Ok(reply["result"].clone())
}

/* A home's "devices" (own) and "receivedDevices" (shared with this
 * account); the model name from "products". */
pub fn parse_home(home: &Value) -> Vec<CloudVacuum> {
    let models: BTreeMap<String, String> = home["products"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["id"].as_str()?.to_string(), p["name"].as_str().or(p["model"].as_str())?.to_string())))
        .collect();
    ["devices", "receivedDevices"]
        .iter()
        .flat_map(|list| home[*list].as_array().cloned().unwrap_or_default())
        .filter_map(|d| {
            Some(CloudVacuum {
                duid: d["duid"].as_str()?.to_string(),
                name: d["name"].as_str().unwrap_or("Roborock").to_string(),
                model: d["productId"].as_str().and_then(|p| models.get(p)).cloned().unwrap_or_default(),
                local_key: d["localKey"].as_str()?.to_string(),
                protocol: d["pv"].as_str().unwrap_or("1.0").to_string(),
                online: d["online"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hawk_signature_matches_python_roborock() {
        /* The mac: python-roborock's _get_hawk_authentication formula,
         * run in Python for these made-up keys, fixed time and nonce. */
        let session = Session {
            base_url: String::new(),
            client: String::new(),
            token: String::new(),
            u: "user1".into(),
            s: "sss".into(),
            h: "hkey".into(),
            k: String::new(),
            api: String::new(),
            mqtt: String::new(),
            logged_in: 0,
        };
        let header = hawk(&session, "/v3/user/homes/123", 1_700_000_000, "abcdefgh");
        assert_eq!(
            header,
            "Hawk id=\"user1\",s=\"sss\",ts=\"1700000000\",nonce=\"abcdefgh\",mac=\"tLsIXPGJnJffrczGA73OBQyAGhEig8fE2T5xHUUl5Vo=\""
        );
    }

    #[test]
    fn a_home_lists_own_and_shared_vacuums() {
        let home = json!({
            "products": [{"id": "p1", "name": "Roborock S7"}],
            "devices": [{"duid": "d1", "name": "S7", "localKey": "k1", "productId": "p1", "pv": "1.0", "online": true}],
            "receivedDevices": [{"duid": "d2", "name": "Q7", "localKey": "k2", "pv": "B01"}, {"name": "no key"}]
        });
        let list = parse_home(&home);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].model, "Roborock S7");
        assert_eq!(list[1].protocol, "B01");
        assert!(!list[1].online);
    }
}
