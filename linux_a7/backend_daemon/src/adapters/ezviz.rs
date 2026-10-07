/*
 * ezviz.rs -- EZVIZ smart plugs (T30 and alike) through EZVIZ's cloud
 * (issue #74, pattern P5: CLOUD ONLY).
 *
 * These plugs have no local API at all: the EZVIZ app itself switches
 * them through EZVIZ's servers. So the hub does the same -- and the plug
 * works only while the hub has internet (its template says so, and the
 * screens show it). Unofficial API, as the EZVIZ app uses it; followed
 * from pyEzvizApi (Apache-2.0).
 *
 * THE ACCOUNT. The wizard asks for the EZVIZ email and password once:
 *   login       POST /v3/users/login/v5 (password as MD5, as the app
 *               sends it). Code 6002 = two-step verification is on: EZVIZ
 *               is asked to send a code (/v3/sms/nologin/checkcode), and
 *               the same login is repeated with it (msgType 3).
 *               Code 1100 = the account lives in another region: retried
 *               there (loginArea.apiDomain).
 *   ->          a session: `sessionId` (sent as a header on every call)
 *               and `rfSessionId` (to get a new session when it expires).
 * The PASSWORD IS NOT KEPT (the wizard forgets it after the step): only
 * the session, saved with the ACCOUNT (accounts.rs; the plug's config
 * says which: "account"). When it expires the
 * task renews it (PUT /v3/apigateway/login) and saves the new one; when
 * EZVIZ refuses that too (password changed, session revoked), the plug
 * shows "unauthorized" and "Pair again" logs in anew.
 *
 * SEVERAL PLUGS, ONE ACCOUNT: each keeps a copy of the session, but a
 * renewal makes the old refresh id useless -- so the adapter keeps the
 * newest session per account in memory (SESSIONS), and every task uses
 * and saves that one.
 *
 * THE PLUG: listed in GET /v3/userdevices/v1/resources/pagelist (filter
 * CLOUD, SWITCH): "deviceInfos" (serial, name, status 1 = online) and
 * "SWITCH" {serial: [{type, enable}]} -- type 14 is the plug's relay.
 * Switched with PUT /v3/devices/<serial>/0/<1|0>/14/switchStatus.
 *
 * CONSUMPTION (the T30-10B measures; not in pyEzvizApi -- found in the
 * EZVIZ app's own plug screen, its "DeviceSocket" plugin):
 *   GET /v3/smarthome/outlet/v1/info/op?deviceSerial=  -> "power": watts
 *       now, "time": seconds on today
 *   GET /v3/smarthome/outlet/records/{hour|day|month}/from/<from>/to/<to>
 *       ?deviceSerial=  -> "data": [{"day"|"hour"|"month", "eleAmount"}]
 *       (kWh), from/to as "2026-10-03", "2026-10-03 21", "2026-10"
 * reported as the `energy` capability: power_w, today_kwh (today's day
 * record), energy_kwh (the last 12 months' records together). A plug
 * that doesn't measure (T30-10A) answers without "power": it gets none.
 */
use hyper::Method;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

use super::cloud::{https_json, CloudRequest};
use super::{Adapter, BoxFuture, CloudDevice, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health};
use crate::secrets::Secret;
use crate::templates::ErrorKind;

/* Where an account is asked first (EU); EZVIZ redirects to its region. */
const DEFAULT_API: &str = "apiieu.ezvizlife.com";
/* The plug's relay among a device's "switches". */
const PLUG_SWITCH: i64 = 14;
/* Its other switches, offered as the `options` capability (the EZVIZ
 * app's plug settings): (type, option id, name). */
const OPTIONS: [(i64, &str, &str); 2] = [(3, "status_light", "Status light"), (600, "power_recovery", "Restore after a power cut")];
/* How often the plug's state is read: someone may use the EZVIZ app or the
 * button on the plug. A cloud API: not too often. */
const POLL: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(300);
/* The first retry after a failure: soon, since the usual cause is the
 * network not being up yet at boot (DNS fails for a few seconds). */
const RETRY_FIRST: Duration = Duration::from_secs(5);

pub struct Ezviz;

/* A logged-in EZVIZ session (the plug's secret "cloud_session"). */
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    pub api: String,
    pub session_id: String,
    pub refresh_id: String,
    /* This hub's client id at EZVIZ (made up at the first login, kept). */
    pub feature_code: String,
    /* Which account (the email): sessions are shared per account. */
    pub account: String,
    /* When it was renewed (Unix s): the newest copy wins. */
    #[serde(default)]
    pub renewed: u64,
}

/* The newest session per account (see the top of the file). */
fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/* Keeps `session` if it's newer than what's known; returns the newest. */
fn newest(session: Session) -> Session {
    let mut all = sessions().lock().unwrap();
    let entry = all.entry(session.account.clone()).or_insert_with(|| session.clone());
    if session.renewed > entry.renewed {
        *entry = session;
    }
    entry.clone()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/* The headers the EZVIZ Android app sends (pyEzvizApi's profile). */
fn headers(feature_code: &str, session_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("featureCode", feature_code.to_string()),
        ("clientType", "3".into()),
        ("osVersion", "13".into()),
        ("clientVersion", "7.4.1.0421".into()),
        ("netType", "WIFI".into()),
        ("customno", "1000001".into()),
        ("clientNo", "google".into()),
        ("appId", "ys7".into()),
        ("language", "en_GB".into()),
        ("lang", "en".into()),
        ("sessionId", session_id.to_string()),
        ("User-Agent", "okhttp/3.12.1".into()),
    ]
}

fn code(reply: &Value) -> i64 {
    reply["meta"]["code"]
        .as_i64()
        .or_else(|| reply["meta"]["code"].as_str().and_then(|c| c.parse().ok()))
        .or_else(|| reply["resultCode"].as_str().and_then(|c| c.parse().ok()))
        .unwrap_or(-1)
}

fn vendor(text: impl Into<String>) -> SetupError {
    SetupError::new(ErrorKind::Vendor, text)
}

async fn call(method: Method, url: &str, request: CloudRequest<'_>) -> Result<Value, SetupError> {
    https_json(method, url, request)
        .await
        .map_err(|e| vendor(format!("Can't reach EZVIZ's servers ({e}). Is the hub online?")))
}

/* ------------------------------------------------------------------ */
/* The account                                                         */
/* ------------------------------------------------------------------ */

/* What a login attempt gave. */
pub enum Login {
    In(Session),
    /* Two-step verification: EZVIZ sent a code; log in again with it. */
    NeedsCode { api: String },
}

/* One login; `code`: the verification code, the second time. */
pub async fn login(account: &str, password: &str, code_sent: Option<&str>, api: &str, feature_code: &str) -> Result<Login, SetupError> {
    let mut api = api.to_string();
    /* At most one redirect to another region. */
    for _ in 0..2 {
        let password_md5 = hex(&Md5::digest(password.as_bytes()));
        let form = [
            ("account", account.to_string()),
            ("password", password_md5),
            ("featureCode", feature_code.to_string()),
            ("msgType", if code_sent.is_some() { "3" } else { "0" }.to_string()),
            ("bizType", if code_sent.is_some() { "TERMINAL_BIND" } else { "" }.to_string()),
            ("cuName", "aHVi".to_string()),
            ("smsCode", code_sent.unwrap_or("").to_string()),
        ];
        let login_headers = headers(feature_code, "");
        let reply = call(
            Method::POST,
            &format!("https://{api}/v3/users/login/v5"),
            CloudRequest { form: Some(&form), headers: &login_headers, ..Default::default() },
        )
        .await?;
        match code(&reply) {
            200 => {
                let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                return Ok(Login::In(Session {
                    api: text(&reply["loginArea"]["apiDomain"]).trim_matches('"').to_string(),
                    session_id: text(&reply["loginSession"]["sessionId"]),
                    refresh_id: text(&reply["loginSession"]["rfSessionId"]),
                    feature_code: feature_code.to_string(),
                    account: account.to_string(),
                    renewed: now(),
                }));
            }
            1100 => {
                api = reply["loginArea"]["apiDomain"].as_str().unwrap_or(DEFAULT_API).to_string();
            }
            6002 => {
                /* Two-step verification: have the code sent. */
                let form = [("from", account.to_string()), ("bizType", "TERMINAL_BIND".to_string())];
                let code_headers = headers(feature_code, "");
                let sent = call(
                    Method::POST,
                    &format!("https://{api}/v3/sms/nologin/checkcode"),
                    CloudRequest { form: Some(&form), headers: &code_headers, ..Default::default() },
                )
                .await?;
                match code(&sent) {
                    200 => {}
                    /* A code went out moments ago (the first try): it's
                     * still valid. */
                    1041 => return Ok(Login::NeedsCode { api }),
                    other => return Err(vendor(format!("EZVIZ couldn't send the verification code (code {other})"))),
                }
                return Ok(Login::NeedsCode { api });
            }
            1012 => return Err(vendor("That verification code isn't right. Check it, or start over for a new one.")),
            1013 | 1014 => return Err(vendor("EZVIZ doesn't accept this email and password. Check them in the EZVIZ app.")),
            1015 => return Err(vendor("EZVIZ locked this account for now (too many tries). Wait a while, then try again.")),
            other => return Err(vendor(format!("EZVIZ refused the login (code {other})"))),
        }
    }
    Err(vendor("EZVIZ kept redirecting the login to another region"))
}

/* A fresh session from the refresh id; Err(true): EZVIZ refuses it (log in
 * again), Err(false): it couldn't be asked (offline). */
async fn renew(session: &Session) -> Result<Session, bool> {
    let form = [("refreshSessionId", session.refresh_id.clone()), ("featureCode", session.feature_code.clone())];
    let headers = headers(&session.feature_code, &session.session_id);
    let reply = https_json(
        Method::PUT,
        &format!("https://{}/v3/apigateway/login", session.api),
        CloudRequest { form: Some(&form), headers: &headers, ..Default::default() },
    )
    .await
    .map_err(|_| false)?;
    if code(&reply) != 200 {
        return Err(true);
    }
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    Ok(Session {
        session_id: text(&reply["sessionInfo"]["sessionId"]),
        refresh_id: text(&reply["sessionInfo"]["refreshSessionId"]),
        renewed: now(),
        ..session.clone()
    })
}

/* An expired or refused session (EZVIZ's codes for it). */
fn session_expired(reply: &Value) -> bool {
    matches!(code(reply), 401 | 403 | 1001 | 2001) || reply["meta"]["message"].as_str().is_some_and(|m| m.contains("session"))
}

/* The account's devices: (serial, name, online, plug on/off if it has a
 * plug switch), from the page list. */
pub fn parse_devices(page: &Value) -> Vec<(String, String, bool, Option<bool>)> {
    page["deviceInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let serial = d["deviceSerial"].as_str()?.to_string();
            let switches = page["SWITCH"][&serial].as_array();
            let plug = switches
                .into_iter()
                .flatten()
                .find(|s| s["type"].as_i64() == Some(PLUG_SWITCH))
                .and_then(|s| s["enable"].as_bool().or_else(|| s["enable"].as_i64().map(|e| e == 1)));
            Some((serial, d["name"].as_str().unwrap_or("EZVIZ device").to_string(), is_online(&d["status"]), plug))
        })
        .collect()
}

/* "status" 1 = online. EZVIZ sends it as a number, but some replies
 * carry it as a string ("1") -- both count. */
fn is_online(status: &Value) -> bool {
    status.as_i64().or_else(|| status.as_str().and_then(|s| s.trim().parse().ok())) == Some(1)
}

/* GET <path>?deviceSerial=<serial>; the reply if it's a 200. Err(true):
 * the session expired. */
async fn get_for(session: &Session, path: &str, serial: &str) -> Result<Value, (bool, String)> {
    let headers = headers(&session.feature_code, &session.session_id);
    let query = [("deviceSerial", serial.to_string())];
    let reply = https_json(
        Method::GET,
        &format!("https://{}{path}", session.api),
        CloudRequest { query: &query, headers: &headers, ..Default::default() },
    )
    .await
    .map_err(|e| (false, e.to_string()))?;
    if code(&reply) == 200 {
        Ok(reply)
    } else {
        Err((session_expired(&reply), format!("EZVIZ refused {path} (code {})", code(&reply))))
    }
}

/* A date in EZVIZ's record paths ("2026-10-03"), the space of the hour
 * form percent-encoded. */
fn day_of(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    /* Civil date from days since 1970 (Howard Hinnant's algorithm). */
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/* The plug's consumption (see the top of the file): (watts now, today
 * kWh, last 12 months kWh). `days`/`months` from the records, asked
 * less often (`with_records`). None: it doesn't measure. */
async fn consumption(session: &Session, serial: &str, now: u64, with_records: bool) -> Result<Option<(f64, Option<f64>, Option<f64>)>, (bool, String)> {
    let info = get_for(session, "/v3/smarthome/outlet/v1/info/op", serial).await?;
    let number = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
    let Some(power) = number(&info["power"]) else { return Ok(None) };
    if !with_records {
        return Ok(Some((power, None, None)));
    }
    /* "Today" is the plug's own day (its time zone; the hub's clock is
     * UTC): asked yesterday to tomorrow (UTC), the newest day is it. */
    let (from, to) = (day_of(now.saturating_sub(86_400)), day_of(now + 86_400));
    let days = get_for(session, &format!("/v3/smarthome/outlet/records/day/from/{from}/to/{to}"), serial).await?;
    let today_kwh = days["data"].as_array().and_then(|d| d.last()).and_then(|d| number(&d["eleAmount"]));
    let this_month = to[..7].to_string();
    let year_ago = day_of(now.saturating_sub(335 * 86_400));
    let months = get_for(session, &format!("/v3/smarthome/outlet/records/month/from/{}/to/{this_month}", &year_ago[..7]), serial).await?;
    let total: Option<f64> = months["data"].as_array().map(|m| m.iter().filter_map(|m| number(&m["eleAmount"])).sum());
    Ok(Some((power, today_kwh, total)))
}

/* GET the page list (CLOUD + SWITCH). Err(true): the session expired. */
async fn page(session: &Session) -> Result<Value, (bool, String)> {
    let headers = headers(&session.feature_code, &session.session_id);
    let query = [
        ("groupId", "-1".to_string()),
        ("limit", "50".to_string()),
        ("offset", "0".to_string()),
        ("filter", "CLOUD,SWITCH".to_string()),
    ];
    let reply = https_json(
        Method::GET,
        &format!("https://{}/v3/userdevices/v1/resources/pagelist", session.api),
        CloudRequest { query: &query, headers: &headers, ..Default::default() },
    )
    .await
    .map_err(|e| (false, e.to_string()))?;
    if code(&reply) == 200 {
        Ok(reply)
    } else {
        Err((session_expired(&reply), format!("EZVIZ refused the device list (code {})", code(&reply))))
    }
}

/* The plug's options (OPTIONS) as the page list says them. */
pub fn options_of(page: &Value, serial: &str) -> Vec<Value> {
    let switches = page["SWITCH"][serial].as_array().cloned().unwrap_or_default();
    OPTIONS
        .iter()
        .filter_map(|(kind, id, name)| {
            let s = switches.iter().find(|s| s["type"].as_i64() == Some(*kind))?;
            let on = s["enable"].as_bool().or_else(|| s["enable"].as_i64().map(|e| e == 1))?;
            Some(json!({"id": id, "name": name, "on": on}))
        })
        .collect()
}

/* Sets one of the plug's switches: the relay (PLUG_SWITCH) or an option.
 * Err(true): the session expired. */
async fn switch(session: &Session, serial: &str, kind: i64, on: bool) -> Result<(), (bool, String)> {
    let headers = headers(&session.feature_code, &session.session_id);
    let reply = https_json(
        Method::PUT,
        &format!("https://{}/v3/devices/{serial}/0/{}/{kind}/switchStatus", session.api, u8::from(on)),
        CloudRequest { headers: &headers, ..Default::default() },
    )
    .await
    .map_err(|e| (false, e.to_string()))?;
    if code(&reply) == 200 {
        Ok(())
    } else {
        Err((session_expired(&reply), format!("EZVIZ refused the switch (code {})", code(&reply))))
    }
}

/* ------------------------------------------------------------------ */
/* The adapter                                                         */
/* ------------------------------------------------------------------ */

impl Adapter for Ezviz {
    fn id(&self) -> &'static str {
        "ezviz"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        /* The account's session (Hub::cloud_session). */
        let account = device.config.get("account").cloned();
        let session = hub
            .cloud_session(device)
            .and_then(|s| serde_json::from_str::<Session>(&s).ok());
        let task = Task {
            id: device.id.clone(),
            account,
            energy: (None, None),
            records_at: None,
            measures: None,
            has_options: false,
            listed_online: None,
            serial: device.config.get("serial").cloned().unwrap_or_default(),
            session: session.map(newest),
            hub,
        };
        let refresh = Arc::new(Notify::new());
        tokio::spawn(task.run(commands_rx, refresh.clone()));
        DeviceHandle::new(commands).with_refresh(refresh)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move {
            /* The account's session (the wizard hands it over), or the
             * device's own older copy. */
            let session: Session = values
                .secret
                .get("account_session")
                .or_else(|| values.secret.get("cloud_session"))
                .and_then(|s| serde_json::from_str(s.expose()).ok())
                .ok_or_else(|| vendor("not logged in to EZVIZ"))?;
            let serial = values.plain.get("serial").cloned().unwrap_or_default();
            let page = page(&session).await.map_err(|(_, e)| vendor(e))?;
            let (_, name, online, plug) = parse_devices(&page)
                .into_iter()
                .find(|d| d.0 == serial)
                .ok_or_else(|| vendor("That plug isn't in the EZVIZ account any more."))?;
            let state = match (online, plug) {
                (false, _) => "offline in the EZVIZ app".to_string(),
                (true, Some(on)) => if on { "on" } else { "off" }.to_string(),
                (true, None) => "no plug switch found".to_string(),
            };
            Ok(Probe {
                values: Default::default(),
                name: Some(name.clone()),
                summary: format!("{name}: {state} (through EZVIZ's cloud)"),
            })
        })
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(action(name, values))
    }
}

async fn action(name: &str, values: &SetupValues) -> Result<SetupValues, SetupError> {
    let mut out = SetupValues::default();
    let plain = |key: &str| values.plain.get(key).cloned().unwrap_or_default();
    let password = values.secret.get("password").map(|p| p.expose().to_string()).unwrap_or_default();
    /* This hub's id at EZVIZ: random, made once per login and kept in the
     * session. */
    let feature_code = || {
        let mut bytes = [0u8; 16];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
        hex(&bytes)
    };
    let keep = |out: &mut SetupValues, session: &Session| {
        out.secret.insert("login_session".into(), Secret::new(serde_json::to_string(session).unwrap_or_default()));
        out.plain.insert("login_needs_code".into(), "no".into());
    };
    match name {
        "login" => {
            let code = feature_code();
            match login(&plain("email"), &password, None, DEFAULT_API, &code).await? {
                Login::In(session) => keep(&mut out, &session),
                Login::NeedsCode { api } => {
                    out.plain.insert("login_needs_code".into(), "yes".into());
                    out.plain.insert("login_api".into(), api);
                    out.plain.insert("login_feature".into(), code);
                }
            }
        }
        "login_code" => {
            let api = values.plain.get("login_api").cloned().unwrap_or_else(|| DEFAULT_API.into());
            match login(&plain("email"), &password, Some(&plain("code")), &api, &plain("login_feature")).await? {
                Login::In(session) => keep(&mut out, &session),
                Login::NeedsCode { .. } => return Err(vendor("EZVIZ asked for another code. Start over.")),
            }
        }
        "list_devices" => {
            let session: Session = values
                .secret
                .get("login_session")
                .and_then(|s| serde_json::from_str(s.expose()).ok())
                .ok_or_else(|| vendor("not logged in to EZVIZ"))?;
            let page = page(&session).await.map_err(|(_, e)| vendor(e))?;
            for (serial, name, online, plug) in parse_devices(&page) {
                out.cloud_devices.push(CloudDevice {
                    id: serial.clone(),
                    name,
                    detail: match plug {
                        Some(_) => format!("Plug {serial} - {}", if online { "online" } else { "offline" }),
                        None => format!("{serial}: not a plug (cameras come with #43)"),
                    },
                    available: plug.is_some(),
                    /* P5: the account's session (accounts.rs) is how the
                     * plug is reached; the device keeps "account". */
                    plain: [("serial".to_string(), serial)].into(),
                    secret: Default::default(),
                });
            }
        }
        other => return Err(SetupError::new(ErrorKind::Unsupported, format!("ezviz has no action {other:?}"))),
    }
    Ok(out)
}

/* One plug's task: reads its state every POLL, switches it, renews the
 * session when EZVIZ says it expired. */
struct Task {
    id: String,
    /* Its account (accounts.rs): where a renewed session is saved. */
    account: Option<String>,
    serial: String,
    /* Consumption: last read today/total (asked every RECORDS_EVERY), and
     * whether the plug measures at all (None: not asked yet). */
    energy: (Option<f64>, Option<f64>),
    records_at: Option<std::time::Instant>,
    measures: Option<bool>,
    has_options: bool,
    /* What the last page list said (logs the raw entry when it turns offline). */
    listed_online: Option<bool>,
    session: Option<Session>,
    hub: Hub,
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>, refresh: Arc<Notify>) {
        if self.session.is_none() || self.serial.is_empty() {
            println!("ezviz: {}: no EZVIZ session: Pair again", self.id);
            self.hub.set_online(&self.id, Health::Unauthorized).await;
            while let Some(cmd) = commands.recv().await {
                cmd.refuse("log in to EZVIZ again (Pair again)");
            }
            return;
        }
        let mut wait = Duration::ZERO;
        let mut retry = RETRY_FIRST;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    wait = match self.poll().await {
                        Ok(()) => {
                            retry = RETRY_FIRST;
                            POLL
                        }
                        /* 5, 10, 20 ... s, up to RETRY_MAX. */
                        Err(()) => {
                            let now = retry;
                            retry = (retry * 2).min(RETRY_MAX);
                            now
                        }
                    };
                }
                cmd = commands.recv() => match cmd {
                    None => return,
                    Some(cmd) => {
                        self.handle(cmd).await;
                        wait = Duration::from_secs(2);
                    }
                },
                /* "Read now" (issue #99: did the device it powers really
                 * switch?): the next poll at once. */
                _ = refresh.notified() => wait = Duration::ZERO,
            }
        }
    }

    /* The session to use: the newest any plug of this account has. */
    fn session(&mut self) -> Session {
        let session = newest(self.session.clone().unwrap());
        self.session = Some(session.clone());
        session
    }

    /* After "expired": renews and saves; false if EZVIZ wants a new login
     * (then "unauthorized"). */
    async fn renew(&mut self) -> bool {
        let current = self.session();
        match renew(&current).await {
            Ok(session) => {
                let session = newest(session);
                self.session = Some(session.clone());
                let json = serde_json::to_string(&session).unwrap_or_default();
                match &self.account {
                    Some(account) => self.hub.store_account_session(account, json),
                    None => self.hub.store_secrets(&self.id, [("cloud_session".to_string(), Secret::new(json))].into()),
                }
                println!("ezviz: {}: session renewed", self.id);
                true
            }
            Err(true) => {
                println!("ezviz: {}: EZVIZ refused the session: Pair again", self.id);
                self.hub.set_online(&self.id, Health::Unauthorized).await;
                false
            }
            Err(false) => false,
        }
    }

    async fn poll(&mut self) -> Result<(), ()> {
        let mut renewed = false;
        loop {
            let session = self.session();
            match page(&session).await {
                Ok(page) => {
                    let Some((_, _, online, plug)) = parse_devices(&page).into_iter().find(|d| d.0 == self.serial) else {
                        println!("ezviz: {}: plug {} isn't in the account any more", self.id, self.serial);
                        self.hub.set_online(&self.id, Health::Offline).await;
                        return Err(());
                    };
                    if let Some(on) = plug {
                        let _ = self.hub.report(&self.id, "switch", json!({ "on": on })).await;
                    }
                    /* Its settings (status light, power-cut restore). */
                    let options = options_of(&page, &self.serial);
                    if !options.is_empty() {
                        if !self.has_options {
                            let _ = self.hub.add_capabilities(&self.id, &["options"]).await;
                            self.has_options = true;
                        }
                        let _ = self.hub.report(&self.id, "options", json!({ "options": options })).await;
                    }
                    if !online && self.listed_online != Some(false) {
                        /* Show what EZVIZ really sent, so a wrong "offline"
                         * can be checked against the app. */
                        let raw = page["deviceInfos"].as_array().and_then(|a| a.iter().find(|d| d["deviceSerial"] == self.serial.as_str())).cloned();
                        println!("ezviz: {}: listed offline: {}", self.id, raw.unwrap_or_default());
                    }
                    self.listed_online = Some(online);
                    self.hub.set_online(&self.id, if online { Health::Online } else { Health::Offline }).await;
                    if online && self.measures != Some(false) {
                        self.read_consumption(&session).await;
                    }
                    return Ok(());
                }
                Err((true, _)) if !renewed => {
                    renewed = true;
                    if !self.renew().await {
                        return Err(());
                    }
                }
                Err((_, e)) => {
                    println!("ezviz: {}: {e}", self.id);
                    self.hub.set_online(&self.id, Health::Offline).await;
                    return Err(());
                }
            }
        }
    }

    /* The plug's watts (every poll) and kWh (every RECORDS_EVERY); a plug
     * that measures gets the `energy` capability the first time. A failure
     * here never makes the plug "offline": switching still works. */
    async fn read_consumption(&mut self, session: &Session) {
        const RECORDS_EVERY: Duration = Duration::from_secs(600);
        let with_records = self.records_at.is_none_or(|t| t.elapsed() >= RECORDS_EVERY);
        match consumption(session, &self.serial, now(), with_records).await {
            Ok(Some((power, today, total))) => {
                if self.measures.is_none() {
                    println!("ezviz: {}: measures power ({power} W now)", self.id);
                    let _ = self.hub.add_capabilities(&self.id, &["energy"]).await;
                }
                self.measures = Some(true);
                if with_records {
                    self.records_at = Some(std::time::Instant::now());
                    self.energy = (today, total);
                }
                let mut value = json!({ "power_w": (power * 10.0).round() / 10.0 });
                if let Some(today) = self.energy.0 {
                    value["today_kwh"] = json!(today);
                }
                if let Some(total) = self.energy.1 {
                    value["energy_kwh"] = json!((total * 100.0).round() / 100.0);
                }
                if let Err(e) = self.hub.report(&self.id, "energy", value).await {
                    println!("ezviz: {}: {e}", self.id);
                }
            }
            Ok(None) => {
                println!("ezviz: {}: doesn't measure power", self.id);
                self.measures = Some(false);
            }
            Err((_, e)) => println!("ezviz: {}: consumption: {e}", self.id),
        }
    }

    async fn handle(&mut self, cmd: DeviceCmd) {
        /* The relay (a command) or an option (the action "set"): both
         * one of the plug's switches. */
        let (kind, on, reply): (i64, bool, super::Reply) = match cmd {
            DeviceCmd::Command { capability, value, reply } if capability == "switch" && value["on"].is_boolean() => {
                (PLUG_SWITCH, value["on"].as_bool().unwrap_or(false), Box::new(move |r| { let _ = reply.send(r); }))
            }
            DeviceCmd::Action { capability, name, args, reply } if capability == "options" && name == "set" => {
                let id = args["id"].as_str().unwrap_or_default();
                let Some((kind, _, _)) = OPTIONS.iter().find(|(_, o, _)| *o == id) else {
                    let _ = reply.send(Err(format!("an EZVIZ plug has no option {id:?}")));
                    return;
                };
                (*kind, args["on"].as_bool().unwrap_or(false), Box::new(move |r: Result<(), String>| { let _ = reply.send(r.map(|_| json!({}))); }))
            }
            other => {
                other.refuse("an EZVIZ plug switches on and off, and sets its options");
                return;
            }
        };
        let mut renewed = false;
        let result = loop {
            let session = self.session();
            match switch(&session, &self.serial, kind, on).await {
                Ok(()) => {
                    if kind == PLUG_SWITCH {
                        let _ = self.hub.report(&self.id, "switch", json!({ "on": on })).await;
                    }
                    /* EZVIZ only accepts a switch the plug answers, so
                     * the plug is online whatever the list said. */
                    self.hub.set_online(&self.id, Health::Online).await;
                    break Ok(());
                }
                Err((true, _)) if !renewed => {
                    renewed = true;
                    if !self.renew().await {
                        break Err("EZVIZ ended the session: Pair again".to_string());
                    }
                }
                Err((_, e)) => break Err(e),
            }
        };
        reply(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_for_record_paths() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_791_059_229), "2026-10-03");
        assert_eq!(day_of(951_782_400), "2000-02-29");
    }

    #[test]
    fn online_status_as_number_or_string() {
        assert!(is_online(&json!(1)));
        assert!(is_online(&json!("1")));
        assert!(!is_online(&json!(2)));
        assert!(!is_online(&json!("2")));
        assert!(!is_online(&Value::Null));
    }

    #[test]
    fn plugs_are_found_in_the_page_list() {
        let page = json!({
            "deviceInfos": [
                {"deviceSerial": "Q12345678", "name": "Kettle plug", "status": 1},
                {"deviceSerial": "C87654321", "name": "Garden camera", "status": 2}
            ],
            "SWITCH": {
                "Q12345678": [{"type": 14, "enable": true}, {"type": 7, "enable": false}],
                "C87654321": [{"type": 7, "enable": true}]
            }
        });
        let devices = parse_devices(&page);
        assert_eq!(devices[0], ("Q12345678".into(), "Kettle plug".into(), true, Some(true)));
        /* A camera: no plug switch, offline. */
        assert_eq!(devices[1], ("C87654321".into(), "Garden camera".into(), false, None));
    }

    /* The project's T30-10B's real SWITCH list (2026-10-03). */
    #[test]
    fn options_come_from_the_switch_list() {
        let page = json!({"SWITCH": {"Q15040044": [
            {"channelNo": 0, "deviceSerial": "Q15040044", "enable": true, "type": 3},
            {"channelNo": 0, "deviceSerial": "Q15040044", "enable": true, "type": 14},
            {"channelNo": 0, "deviceSerial": "Q15040044", "enable": false, "type": 600}]}});
        assert_eq!(
            options_of(&page, "Q15040044"),
            vec![json!({"id": "status_light", "name": "Status light", "on": true}),
                 json!({"id": "power_recovery", "name": "Restore after a power cut", "on": false})]
        );
        assert!(options_of(&page, "other").is_empty());
    }

    #[test]
    fn the_newest_session_wins() {
        let session = |renewed, id: &str| Session {
            api: "a".into(),
            session_id: id.into(),
            refresh_id: "r".into(),
            feature_code: "f".into(),
            account: "test-newest@example.com".into(),
            renewed,
        };
        assert_eq!(newest(session(10, "old")).session_id, "old");
        assert_eq!(newest(session(20, "new")).session_id, "new");
        /* A plug starting with an older copy gets the newest. */
        assert_eq!(newest(session(10, "old")).session_id, "new");
    }

    #[test]
    fn codes_are_read_both_ways() {
        assert_eq!(code(&json!({"meta": {"code": 200}})), 200);
        assert_eq!(code(&json!({"meta": {"code": "6002"}})), 6002);
        assert_eq!(code(&json!({"resultCode": "0"})), 0);
        assert!(session_expired(&json!({"meta": {"code": 401}})));
    }
}
