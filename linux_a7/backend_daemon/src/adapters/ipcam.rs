/*
 * ipcam.rs -- any camera with a stream (issue #43): RTSP/ONVIF cameras
 * (Reolink, Hikvision, Dahua, Tapo without its settings, ...) and
 * ESP32-CAMs. Adapter "camera", capability `camera` (camera.rs does the
 * pictures).
 *
 * Settings (config):
 *   kind "rtsp"      host, port (554), path ("/stream2", what the maker
 *                    documents), stream_user + secret stream_password --
 *                    or one secret "url" (the whole RTSP URL, "Enter the
 *                    stream URL")
 *   kind "esp32cam"  host: the CameraWebServer example's /capture (one
 *                    JPEG, nothing to decode) and :81/stream (MJPEG, for
 *                    apps)
 * The task only checks the camera answers (a TCP connect every minute):
 * it reads no video until someone asks for a picture.
 *
 * ONVIF (onvif.rs), with the same user name and password, when the camera
 * offers it: pan/tilt and preset positions (the `camera` capability's
 * ptz, presets; actions move, preset), and MOTION, kept subscribed to and
 * reported as a `sensor` reading "motion" (1 = motion now, 0 = none) --
 * usable by automations. A camera without ONVIF just doesn't get them.
 */
use base64::Engine;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::camera::{userinfo, Snapshots, Source};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health};
use crate::templates::ErrorKind;

const CHECK_EVERY: Duration = Duration::from_secs(60);

/* A camera's addresses: (stream URL for apps and pictures, a picture URL
 * if it serves single pictures, the host:port to check). */
pub fn addresses(config: &BTreeMap<String, String>, secret: impl Fn(&str) -> Option<String>) -> Option<(String, Option<String>, String)> {
    let host = config.get("host").cloned().unwrap_or_default();
    match config.get("kind").map(String::as_str).unwrap_or("rtsp") {
        "esp32cam" if !host.is_empty() => Some((
            format!("http://{host}:81/stream"),
            Some(format!("http://{host}/capture")),
            format!("{host}:80"),
        )),
        "esp32cam" => None,
        _ => {
            if let Some(url) = secret("url").filter(|u| u.starts_with("rtsp://") || u.starts_with("http://")) {
                let target = url.split("://").nth(1)?.split('/').next()?.rsplit('@').next()?.to_string();
                let target = if target.contains(':') { target } else { format!("{target}:554") };
                return Some((url, None, target));
            }
            if host.is_empty() {
                return None;
            }
            let port = config.get("port").cloned().unwrap_or_else(|| "554".into());
            let path = config.get("path").cloned().unwrap_or_else(|| "/".into());
            let path = if path.starts_with('/') { path } else { format!("/{path}") };
            let user = config.get("stream_user").cloned().unwrap_or_default();
            let password = secret("stream_password").unwrap_or_default();
            Some((format!("rtsp://{}{host}:{port}{path}", userinfo(&user, &password)), None, format!("{host}:{port}")))
        }
    }
}

/* ONVIF's host and login: the stream account (or the user:password in a
 * whole stream URL, percent-decoded). */
pub fn onvif_login(config: &BTreeMap<String, String>, secret: impl Fn(&str) -> Option<String>) -> Option<(String, String, String)> {
    if config.get("kind").map(String::as_str) == Some("esp32cam") {
        return None;
    }
    if let Some(url) = secret("url") {
        let rest = url.split("://").nth(1)?;
        let authority = rest.split('/').next()?;
        let (userinfo, hostport) = authority.rsplit_once('@').unwrap_or(("", authority));
        let host = hostport.rsplit_once(':').map_or(hostport, |(h, _)| h).to_string();
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        return Some((host, percent_decode(user), percent_decode(password)));
    }
    Some((
        config.get("host")?.clone(),
        config.get("stream_user").cloned().unwrap_or_default(),
        secret("stream_password").unwrap_or_default(),
    ))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub struct IpCam;

impl Adapter for IpCam {
    fn id(&self) -> &'static str {
        "camera"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let secrets = hub.secrets(&device.id);
        let found = addresses(&device.config, |n| secrets.get(n).map(|s| s.expose().to_string()));
        let login = onvif_login(&device.config, |n| secrets.get(n).map(|s| s.expose().to_string()));
        tokio::spawn(run(device.id.clone(), found, login, hub, commands_rx));
        DeviceHandle::new(commands)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move {
            let (stream, picture, _) = addresses(&values.plain, |n| values.secret.get(n).map(|s| s.expose().to_string()))
                .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "no camera address"))?;
            let snapshots = Snapshots::new(match picture {
                Some(url) => Source::Jpeg(url),
                None => Source::Stream(stream),
            });
            let jpeg = snapshots.get(false).await.map_err(|e| SetupError::new(ErrorKind::Unreachable, e))?;
            Ok(Probe {
                values: Default::default(),
                name: None,
                summary: format!("Picture OK ({} KB)", jpeg.len() / 1024),
            })
        })
    }
}

type Shared = Arc<tokio::sync::Mutex<Option<super::onvif::Onvif>>>;

/* ONVIF in the background: finds it (again every RETRY until it works),
 * reports pan/tilt and presets, then keeps the motion subscription. */
async fn onvif_task(id: String, login: (String, String, String), hub: Hub, shared: Shared) {
    const RETRY: Duration = Duration::from_secs(300);
    let (host, user, password) = login;
    let cam = loop {
        match super::onvif::Onvif::connect(&host, &user, &password).await {
            Ok(cam) => break cam,
            Err(e) => {
                println!("camera: {id}: no ONVIF ({e}); trying again in {} min", RETRY.as_secs() / 60);
                tokio::time::sleep(RETRY).await;
            }
        }
    };
    let presets = cam.presets().await.unwrap_or_default();
    println!("camera: {id}: ONVIF found (pan/tilt: {}, {} presets, motion events: {})", cam.can_move(), presets.len(), cam.has_events());
    let presets: Vec<serde_json::Value> = presets.into_iter().map(|(token, name)| json!({"token": token, "name": name})).collect();
    let _ = hub.report(&id, "camera", json!({"snapshot": true, "stream": true, "ptz": cam.can_move(), "presets": presets})).await;
    *shared.lock().await = Some(cam.clone());
    if !cam.has_events() {
        return;
    }
    let _ = hub.add_capabilities(&id, &["sensor"]).await;
    let report = |motion: bool| {
        let hub = hub.clone();
        let id = id.clone();
        async move {
            let _ = hub.report(&id, "sensor", json!({"readings": {"motion": {"value": if motion { 1.0 } else { 0.0 }, "unit": ""}}})).await;
        }
    };
    report(false).await;
    loop {
        let subscription = match cam.subscribe().await {
            Ok(s) => s,
            Err(e) => {
                println!("camera: {id}: motion events: {e}");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let mut renewed = std::time::Instant::now();
        loop {
            match cam.pull_motion(&subscription).await {
                Ok(Some(motion)) => report(motion).await,
                Ok(None) => {}
                Err(e) => {
                    println!("camera: {id}: motion events: {e} (subscribing again)");
                    break;
                }
            }
            if renewed.elapsed() > Duration::from_secs(300) {
                if cam.renew(&subscription).await.is_err() {
                    break;
                }
                renewed = std::time::Instant::now();
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn run(
    id: String,
    found: Option<(String, Option<String>, String)>,
    login: Option<(String, String, String)>,
    hub: Hub,
    mut commands: mpsc::Receiver<DeviceCmd>,
) {
    let Some((stream, picture, target)) = found else {
        println!("camera: {id}: no address");
        hub.set_online(&id, Health::Offline).await;
        while let Some(cmd) = commands.recv().await {
            cmd.refuse("the camera has no address set");
        }
        return;
    };
    let snapshots: Arc<Snapshots> = Snapshots::new(match &picture {
        Some(url) => Source::Jpeg(url.clone()),
        None => Source::Stream(stream.clone()),
    });
    /* Live video for the screen finds it by id (camera.rs). */
    super::camera::register(&id, &snapshots);
    let _ = hub.report(&id, "camera", json!({"snapshot": true, "stream": true})).await;
    let onvif: Shared = Arc::new(tokio::sync::Mutex::new(None));
    let onvif_handle = login.map(|login| tokio::spawn(onvif_task(id.clone(), login, hub.clone(), onvif.clone())));
    /* Stops ONVIF when this task ends (the device removed or restarted). */
    struct Abort(Option<tokio::task::JoinHandle<()>>);
    impl Drop for Abort {
        fn drop(&mut self) {
            if let Some(handle) = &self.0 {
                handle.abort();
            }
        }
    }
    let _abort = Abort(onvif_handle);
    let mut check = tokio::time::interval(CHECK_EVERY);
    loop {
        tokio::select! {
            _ = check.tick() => {
                let answers = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&target)).await.is_ok_and(|r| r.is_ok());
                hub.set_online(&id, if answers { Health::Online } else { Health::Offline }).await;
            }
            cmd = commands.recv() => match cmd {
                None => return,
                Some(DeviceCmd::Action { capability, name, args, reply }) if capability == "camera" && (name == "move" || name == "preset") => {
                    let onvif = onvif.clone();
                    tokio::spawn(async move {
                        let cam = onvif.lock().await.clone();
                        let result = match cam {
                            None => Err("the camera's ONVIF isn't reachable (yet)".to_string()),
                            Some(cam) if name == "move" => {
                                cam.step(args["pan"].as_f64().unwrap_or(0.0) as f32, args["tilt"].as_f64().unwrap_or(0.0) as f32).await
                            }
                            Some(cam) => cam.goto_preset(args["token"].as_str().unwrap_or_default()).await,
                        };
                        let _ = reply.send(result.map(|_| json!({})));
                    });
                }
                Some(DeviceCmd::Action { capability, name, args, reply }) if capability == "camera" => {
                    if name == "stream" {
                        let _ = reply.send(Ok(json!({ "url": stream })));
                    } else {
                        let snapshots = snapshots.clone();
                        let live = args["live"].as_bool().unwrap_or(false);
                        tokio::spawn(async move {
                            let result = snapshots
                                .get(live)
                                .await
                                .map(|jpeg| json!({ "jpeg": base64::engine::general_purpose::STANDARD.encode(jpeg) }));
                            let _ = reply.send(result);
                        });
                    }
                }
                Some(other) => other.refuse("a camera gives pictures and its stream address"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onvif_logins() {
        let config = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        assert_eq!(
            onvif_login(&config(&[]), |n| (n == "url").then(|| "rtsp://me:p%40ss@cam.local:554/live".to_string())),
            Some(("cam.local".into(), "me".into(), "p@ss".into()))
        );
        assert_eq!(
            onvif_login(&config(&[("host", "10.0.0.5"), ("stream_user", "admin")]), |n| (n == "stream_password").then(|| "x".into())),
            Some(("10.0.0.5".into(), "admin".into(), "x".into()))
        );
        assert!(onvif_login(&config(&[("kind", "esp32cam"), ("host", "h")]), |_| None).is_none());
    }

    #[test]
    fn camera_addresses() {
        let config = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let (stream, picture, target) = addresses(&config(&[("kind", "esp32cam"), ("host", "192.168.1.60")]), |_| None).unwrap();
        assert_eq!((stream.as_str(), picture.as_deref(), target.as_str()), ("http://192.168.1.60:81/stream", Some("http://192.168.1.60/capture"), "192.168.1.60:80"));
        let rtsp = addresses(&config(&[("host", "10.0.0.5"), ("path", "h264Preview_01_sub"), ("stream_user", "admin")]), |n| {
            (n == "stream_password").then(|| "p@ss".to_string())
        })
        .unwrap();
        assert_eq!(rtsp.0, "rtsp://admin:p%40ss@10.0.0.5:554/h264Preview_01_sub");
        assert_eq!(rtsp.2, "10.0.0.5:554");
        let url = addresses(&config(&[]), |n| (n == "url").then(|| "rtsp://u:p@cam.local:8554/live".to_string())).unwrap();
        assert_eq!(url.2, "cam.local:8554");
    }
}
