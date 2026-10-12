/*
 * onvif.rs -- the ONVIF camera standard (issue #43): pan/tilt, preset
 * positions and motion events, for any ONVIF camera -- a Tapo with its
 * "Camera Account" (port 2020), Reolink, Hikvision, Dahua (port 80).
 *
 * ONVIF is SOAP: XML requests over HTTP, signed with WS-Security's
 * UsernameToken digest -- Base64(SHA-1(nonce + created + password)), the
 * password never sent. "created" must be close to the CAMERA's clock, so
 * its time is asked first (GetSystemDateAndTime needs no login) and the
 * difference kept.
 *
 *   connect      GetCapabilities -> where the Media, PTZ and Events
 *                services are; Media GetProfiles -> the profile token PTZ
 *                commands name
 *   step         PTZ ContinuousMove at a speed, STEP later Stop: one nudge
 *                per tap (Tapo moves in steps of a few degrees)
 *   presets      PTZ GetPresets / GotoPreset
 *   motion       Events CreatePullPointSubscription, then PullMessages in a
 *                loop: tt:...CellMotionDetector/Motion IsMotion true/false
 *
 * The XML is read with a few tolerant helpers (find an element by its
 * local name, whatever its namespace prefix): these answers are small and
 * regular; a full XML library would be a lot of code for that.
 */
use base64::Engine;
use std::time::Duration;

use super::net::http_request_raw;

/* ONVIF's usual ports: Tapo, then everyone else. */
pub const PORTS: [u16; 2] = [2020, 80];
const STEP: Duration = Duration::from_millis(400);

/* The text of the first element with this local name ("XAddr" finds
 * <tt:XAddr>), and the rest of the text after it. */
pub fn element<'a>(xml: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    while let Some(i) = xml[from..].find('<') {
        let start = from + i;
        let tag_end = start + xml[start..].find('>')?;
        let tag = &xml[start + 1..tag_end];
        let tag_name = tag.split([' ', '/', '\t', '\n']).next().unwrap_or("");
        let local = tag_name.rsplit(':').next().unwrap_or("");
        if local == name && !tag.starts_with('/') {
            if tag.ends_with('/') {
                return Some(("", &xml[tag_end + 1..]));
            }
            let close = format!("</{tag_name}>");
            let body_end = tag_end + 1 + xml[tag_end + 1..].find(&close)?;
            return Some((&xml[tag_end + 1..body_end], &xml[body_end + close.len()..]));
        }
        from = tag_end + 1;
    }
    None
}

/* An attribute of the first element with this local name. */
pub fn attribute<'a>(xml: &'a str, element_name: &str, attr: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = xml[from..].find('<') {
        let start = from + i;
        let tag_end = start + xml[start..].find('>')?;
        let tag = &xml[start + 1..tag_end];
        let local = tag.split([' ', '/']).next().unwrap_or("").rsplit(':').next().unwrap_or("");
        if local == element_name {
            let key = format!("{attr}=\"");
            let at = tag.find(&key)? + key.len();
            let end = at + tag[at..].find('"')?;
            return Some(&tag[at..end]);
        }
        from = tag_end + 1;
    }
    None
}

fn sha1(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().to_vec()
}

/* The WS-Security header (UsernameToken digest). */
pub fn security(user: &str, password: &str, created: &str, nonce: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let digest = b64.encode(sha1(&[nonce, created.as_bytes(), password.as_bytes()]));
    format!(
        "<Security s:mustUnderstand=\"1\" xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\">\
<UsernameToken><Username>{}</Username>\
<Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</Password>\
<Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{}</Nonce>\
<Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">{created}</Created>\
</UsernameToken></Security>",
        xml_escape(user),
        b64.encode(nonce)
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/* Unix seconds -> "2026-10-04T10:00:00Z". */
pub fn iso_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/* ONVIF's UTC date/time element -> Unix seconds. */
fn parse_camera_time(xml: &str) -> Option<i64> {
    let (utc, _) = element(xml, "UTCDateTime")?;
    let num = |name: &str, within: &str| element(within, name).and_then(|(v, _)| v.trim().parse::<i64>().ok());
    let (date, _) = element(utc, "Date")?;
    let (time, _) = element(utc, "Time")?;
    let (y, m, d) = (num("Year", date)?, num("Month", date)?, num("Day", date)?);
    let (h, min, s) = (num("Hour", time)?, num("Minute", time)?, num("Second", time)?);
    /* Days from civil (Howard Hinnant). */
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + min * 60 + s)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/* A camera's ONVIF services, found by `connect`. */
#[derive(Clone, Debug)]
pub struct Onvif {
    host: String,
    port: u16,
    user: String,
    password: String,
    /* The camera's clock minus ours (s). */
    offset: i64,
    ptz: Option<String>,
    events: Option<String>,
    profile: Option<String>,
}

/* A service's path from an XAddr ("http://192.168.1.133:2020/onvif/ptz" ->
 * "/onvif/ptz"); the host and port are the ones we reached. */
fn path_of(xaddr: &str) -> String {
    let rest = xaddr.split("://").nth(1).unwrap_or(xaddr);
    rest.find('/').map_or("/".to_string(), |i| rest[i..].to_string())
}

impl Onvif {
    async fn post(&self, path: &str, body: &str, sign: bool) -> Result<String, String> {
        let header = if sign {
            let mut nonce = [0u8; 16];
            let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce);
            format!("<s:Header>{}</s:Header>", security(&self.user, &self.password, &iso_time(now() + self.offset), &nonce))
        } else {
            String::new()
        };
        let envelope = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:tt=\"http://www.onvif.org/ver10/schema\">{header}<s:Body>{body}</s:Body></s:Envelope>"
        );
        let bytes = http_request_raw(&format!("{}:{}", self.host, self.port), path, "application/soap+xml; charset=utf-8", envelope.into_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        if element(&text, "Fault").is_some() {
            let reason = element(&text, "Text").map_or("refused", |(t, _)| t);
            return Err(format!("the camera refused: {reason}"));
        }
        Ok(text)
    }

    /* Finds the camera's ONVIF services (see the top of the file). */
    pub async fn connect(host: &str, user: &str, password: &str) -> Result<Onvif, String> {
        let mut last = String::from("no ONVIF answer");
        for port in PORTS {
            let mut cam = Onvif {
                host: host.to_string(),
                port,
                user: user.to_string(),
                password: password.to_string(),
                offset: 0,
                ptz: None,
                events: None,
                profile: None,
            };
            let time = match cam
                .post("/onvif/device_service", "<GetSystemDateAndTime xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>", false)
                .await
            {
                Ok(xml) => xml,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            if let Some(camera_time) = parse_camera_time(&time) {
                cam.offset = camera_time - now();
            }
            let caps = cam
                .post(
                    "/onvif/device_service",
                    "<GetCapabilities xmlns=\"http://www.onvif.org/ver10/device/wsdl\"><Category>All</Category></GetCapabilities>",
                    true,
                )
                .await?;
            let xaddr = |service: &str| element(&caps, service).and_then(|(s, _)| element(s, "XAddr")).map(|(x, _)| path_of(x.trim()));
            cam.ptz = xaddr("PTZ");
            cam.events = xaddr("Events");
            if let Some(media) = xaddr("Media") {
                let profiles = cam.post(&media, "<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>", true).await?;
                cam.profile = attribute(&profiles, "Profiles", "token").map(str::to_string);
            }
            return Ok(cam);
        }
        Err(last)
    }

    pub fn can_move(&self) -> bool {
        self.ptz.is_some() && self.profile.is_some()
    }

    pub fn has_events(&self) -> bool {
        self.events.is_some()
    }

    /* One nudge: pan/tilt speeds -1..1. */
    pub async fn step(&self, pan: f32, tilt: f32) -> Result<(), String> {
        let (Some(ptz), Some(profile)) = (&self.ptz, &self.profile) else {
            return Err("this camera can't pan or tilt".into());
        };
        let pan = pan.clamp(-1.0, 1.0);
        let tilt = tilt.clamp(-1.0, 1.0);
        self.post(
            ptz,
            &format!(
                "<ContinuousMove xmlns=\"http://www.onvif.org/ver20/ptz/wsdl\"><ProfileToken>{profile}</ProfileToken>\
<Velocity><tt:PanTilt x=\"{pan}\" y=\"{tilt}\"/></Velocity></ContinuousMove>"
            ),
            true,
        )
        .await?;
        tokio::time::sleep(STEP).await;
        self.post(
            ptz,
            &format!("<Stop xmlns=\"http://www.onvif.org/ver20/ptz/wsdl\"><ProfileToken>{profile}</ProfileToken><PanTilt>true</PanTilt></Stop>"),
            true,
        )
        .await
        .map(|_| ())
    }

    /* The saved positions: (token, name). */
    pub async fn presets(&self) -> Result<Vec<(String, String)>, String> {
        let (Some(ptz), Some(profile)) = (&self.ptz, &self.profile) else { return Ok(Vec::new()) };
        let xml = self
            .post(ptz, &format!("<GetPresets xmlns=\"http://www.onvif.org/ver20/ptz/wsdl\"><ProfileToken>{profile}</ProfileToken></GetPresets>"), true)
            .await?;
        Ok(parse_presets(&xml))
    }

    pub async fn goto_preset(&self, token: &str) -> Result<(), String> {
        let (Some(ptz), Some(profile)) = (&self.ptz, &self.profile) else {
            return Err("this camera can't pan or tilt".into());
        };
        self.post(
            ptz,
            &format!(
                "<GotoPreset xmlns=\"http://www.onvif.org/ver20/ptz/wsdl\"><ProfileToken>{profile}</ProfileToken>\
<PresetToken>{}</PresetToken></GotoPreset>",
                xml_escape(token)
            ),
            true,
        )
        .await
        .map(|_| ())
    }

    /* A pull-point subscription for events; its path. */
    pub async fn subscribe(&self) -> Result<String, String> {
        let events = self.events.as_ref().ok_or("the camera has no events")?;
        let xml = self
            .post(
                events,
                "<CreatePullPointSubscription xmlns=\"http://www.onvif.org/ver10/events/wsdl\"><InitialTerminationTime>PT600S</InitialTerminationTime></CreatePullPointSubscription>",
                true,
            )
            .await?;
        let (reference, _) = element(&xml, "SubscriptionReference").ok_or("no subscription")?;
        let (address, _) = element(reference, "Address").ok_or("no subscription address")?;
        Ok(path_of(address.trim()))
    }

    /* The events since the last pull (waiting up to 20 s for one): motion
     * on/off as the camera says it, the last one wins. */
    pub async fn pull_motion(&self, subscription: &str) -> Result<Option<bool>, String> {
        let xml = self
            .post(
                subscription,
                "<PullMessages xmlns=\"http://www.onvif.org/ver10/events/wsdl\"><Timeout>PT20S</Timeout><MessageLimit>32</MessageLimit></PullMessages>",
                true,
            )
            .await?;
        Ok(parse_motion(&xml))
    }

    /* Keeps a subscription alive (PT600S again). */
    pub async fn renew(&self, subscription: &str) -> Result<(), String> {
        self.post(
            subscription,
            "<Renew xmlns=\"http://docs.oasis-open.org/wsn/b-2\"><TerminationTime>PT600S</TerminationTime></Renew>",
            true,
        )
        .await
        .map(|_| ())
    }
}

pub fn parse_presets(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("Preset ") {
        let chunk = &rest[i.saturating_sub(16)..];
        let Some(token) = attribute(chunk, "Preset", "token") else { break };
        let name = element(chunk, "Name").map_or(token, |(n, _)| n).to_string();
        out.push((token.to_string(), name));
        rest = &rest[i + 7..];
    }
    out
}

/* Motion in a PullMessages answer: the last "IsMotion"/"State"/"Motion"
 * SimpleItem's value in a motion topic. */
pub fn parse_motion(xml: &str) -> Option<bool> {
    let mut motion = None;
    let mut rest = xml;
    while let Some((message, after)) = element(rest, "NotificationMessage") {
        rest = after;
        let topic = element(message, "Topic").map_or("", |(t, _)| t);
        if !topic.contains("Motion") && !topic.contains("motion") {
            continue;
        }
        let mut items = message;
        while let Some(i) = items.find("SimpleItem") {
            let chunk = &items[i.saturating_sub(16)..];
            if let (Some(name), Some(value)) = (attribute(chunk, "SimpleItem", "Name"), attribute(chunk, "SimpleItem", "Value")) {
                if matches!(name, "IsMotion" | "State" | "Motion" | "IsMotionDetected") {
                    motion = Some(value == "true" || value == "1");
                }
            }
            items = &items[i + 10..];
        }
    }
    motion
}

#[cfg(test)]
mod tests {
    use super::*;

    /* The digest of the WS-Security spec's own example (ONVIF Application
     * Programmer's Guide): nonce LKqI6G/AikKCQrN0zqZFlg==, created
     * 2010-09-16T07:50:45Z, password userpassword ->
     * tuOSpGlFlIXsozq4HFNeeGeFLEI=. */
    #[test]
    fn password_digest_matches_the_spec() {
        let nonce = base64::engine::general_purpose::STANDARD.decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        let header = security("user", "userpassword", "2010-09-16T07:50:45Z", &nonce);
        assert!(header.contains(">tuOSpGlFlIXsozq4HFNeeGeFLEI=<"), "{header}");
    }

    #[test]
    fn times_round_trip() {
        assert_eq!(iso_time(1_791_059_229), "2026-10-03T20:27:09Z");
        let xml = "<tt:UTCDateTime><tt:Time><tt:Hour>20</tt:Hour><tt:Minute>27</tt:Minute><tt:Second>9</tt:Second></tt:Time>\
<tt:Date><tt:Year>2026</tt:Year><tt:Month>10</tt:Month><tt:Day>3</tt:Day></tt:Date></tt:UTCDateTime>";
        assert_eq!(parse_camera_time(xml), Some(1_791_059_229));
    }

    #[test]
    fn capabilities_presets_and_motion_are_read() {
        let caps = "<tds:Capabilities><tt:Media><tt:XAddr>http://192.168.1.133:2020/onvif/service</tt:XAddr></tt:Media>\
<tt:PTZ><tt:XAddr>http://192.168.1.133:2020/onvif/service</tt:XAddr></tt:PTZ></tds:Capabilities>";
        let ptz = element(caps, "PTZ").and_then(|(s, _)| element(s, "XAddr")).map(|(x, _)| path_of(x)).unwrap();
        assert_eq!(ptz, "/onvif/service");
        let profiles = "<trt:Profiles token=\"profile_1\" fixed=\"true\"><tt:Name>mainStream</tt:Name></trt:Profiles>";
        assert_eq!(attribute(profiles, "Profiles", "token"), Some("profile_1"));
        let presets = "<tptz:Preset token=\"1\"><tt:Name>Door</tt:Name></tptz:Preset><tptz:Preset token=\"2\"><tt:Name>Window</tt:Name></tptz:Preset>";
        assert_eq!(parse_presets(presets), vec![("1".into(), "Door".into()), ("2".into(), "Window".into())]);
        let pull = "<wsnt:NotificationMessage><wsnt:Topic Dialect=\"x\">tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic>\
<wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name=\"IsMotion\" Value=\"true\"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>";
        assert_eq!(parse_motion(pull), Some(true));
        assert_eq!(parse_motion("<x/>"), None);
    }
}
