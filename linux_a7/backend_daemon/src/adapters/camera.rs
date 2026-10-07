/*
 * camera.rs -- pictures from cameras (issues #43, #74), within what the
 * STM32MP157 can do: it has NO video decoder, so the dual Cortex-A7
 * decodes in software -- full-rate video is out of the question, a picture
 * every second or two is not.
 *
 * WHERE A PICTURE COMES FROM (Source):
 *   Jpeg   the camera serves single pictures itself (an ESP32-CAM's
 *          /capture): one HTTP GET, nothing decoded here at all;
 *   Stream an RTSP stream (Tapo's low-resolution "stream2", any ONVIF/RTSP
 *          camera) or an HTTP MJPEG stream: read by ffmpeg, which decodes
 *          ONLY THE KEY FRAMES (-skip_frame nokey: one full picture every
 *          1-2 s in a typical stream; the frames in between are skipped
 *          undecoded) and writes them as JPEG, ~640 px wide.
 *
 * TWO MODES (the request says which): STILL as above -- a sharp picture
 * every key frame, ~5 % CPU on the DK2 (measured, Tapo C200's stream2) --
 * and LIVE: the whole stream decoded, LIVE_FPS pictures a second, for a
 * screen that shows the camera live.
 *
 * STILL PICTURES FROM THE HD STREAM: a camera's small stream (640x360, low
 * bit rate) looks blurry and blocky; its HD stream's key frames, scaled
 * down here, are much sharper -- and decoding one key frame every second
 * or two is affordable even at 1080p (the frames in between are skipped).
 * hd_of() finds the HD stream from the small one's URL for the cameras
 * whose naming is known (Tapo, Reolink, Hikvision, Dahua); if it gives no
 * picture, stills fall back to the small stream for good. LIVE always
 * reads the small stream: decoding EVERY HD frame is beyond the A7. ffmpeg restarts when the mode
 * changes. One decoding thread (-threads 1): plenty for this, and far less
 * memory than ffmpeg's one-per-core.
 *
 * ONLY WHILE SOMEONE LOOKS: ffmpeg starts at the first request and stops
 * IDLE after the last one (a screen showing the camera asks every couple
 * of seconds). Nothing runs for a camera nobody is looking at.
 *
 * LIVE VIDEO ON THE HUB'S SCREEN (2026-10-06; Snapshots::watch_video):
 * pictures as JPEG are too slow for live -- on the DK2, decoding the
 * small stream costs ~1 core and making a JPEG of every frame another
 * full core (measured: 0.83x real time, both cores busy). So for the
 * screen, ffmpeg decodes EVERY frame of the small stream and writes RAW
 * RGB pixels already at the screen's picture size (scaled, letterboxed):
 * nothing to encode here, nothing to decode in the UI. The frames go to
 * the screen over its local connection (ws.rs, binary messages), newest
 * only (a tokio watch channel: a frame the screen had no time for is
 * simply replaced, so the picture never lags behind). ffmpeg runs at a
 * LOW PRIORITY (nice 10): the screen and the hub always come first, the
 * video gets the CPU that's left, on both cores. It stops VIDEO_IDLE
 * after the last viewer leaves. Measured cost: ~1.1 cores of 2 while the
 * camera's page is open.
 *
 * ffmpeg runs as backend_daemon's child, inside the same sandbox
 * (backend-daemon.service); the stream URL with its credentials is in its
 * arguments, which only hubd can see (ProtectProc=invisible). Full-quality
 * video is the APP's job (the "stream" action hands it the URL): a phone
 * decodes in hardware; no video ever goes through the hub or a cloud.
 */
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/* After the last request, ffmpeg stops this much later. */
const IDLE: Duration = Duration::from_secs(30);
/* A first picture must come in this time (connect + the next key frame). */
const FIRST_PICTURE: Duration = Duration::from_secs(12);
/* A picture older than this is stale (the stream stalled). */
const FRESH: Duration = Duration::from_secs(10);
/* Nothing a camera sends us is anywhere near this (a 640 px JPEG is
 * ~30-60 KB): more means a broken stream. */
const MAX_PICTURE: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /* A URL answering one JPEG per GET. */
    Jpeg(String),
    /* An rtsp:// or http:// (MJPEG) stream. */
    Stream(String),
}

/* The newest picture and when it came. */
type Latest = Arc<Mutex<Option<(Instant, Vec<u8>)>>>;

struct Running {
    child: Child,
    latest: Latest,
    live: bool,
    /* The URL ffmpeg reads (HD or the small stream). */
    url: String,
}

/* Pictures a second in live mode. */
const LIVE_FPS: u32 = 4;

/* Live video (see the top of the file): the biggest picture asked for
 * (the DK2's screen), and how long ffmpeg goes on after the last viewer
 * left (a quick back-and-forth keeps it). Every frame the camera sends
 * is shown, at the camera's own rate: making 15 a second out of a
 * camera's 20 dropped every 4th frame, and the motion jumped at that
 * beat (choppy, though 15 a second came). Measured 2026-10-07, Tapo
 * C200 stream2 (1280x720, 20 fps): ~36 % CPU ffmpeg, ~14 % the UI. */
const VIDEO_MAX: (u16, u16) = (800, 480);
const VIDEO_IDLE: Duration = Duration::from_secs(5);
/* Lower than everything else on the hub (0): the video gets what's left. */
const VIDEO_NICE: i32 = 10;
/* How often the frame rate actually reached goes to the journal. */
const FPS_LOG: Duration = Duration::from_secs(10);

/* One video frame: RGB, 3 bytes a pixel, row by row from the top. */
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

/* The newest frame (None until the first one came). */
pub type FrameRx = tokio::sync::watch::Receiver<Option<Arc<Frame>>>;

struct VideoRun {
    child: Child,
    size: (u16, u16),
    frames: tokio::sync::watch::Sender<Option<Arc<Frame>>>,
}

pub struct Snapshots {
    source: Source,
    /* The HD stream for stills (hd_of), None once it failed. */
    hd: Mutex<Option<String>>,
    running: tokio::sync::Mutex<Option<Running>>,
    last_asked: Arc<Mutex<Instant>>,
    /* Live video for the screen, while someone watches. */
    video: tokio::sync::Mutex<Option<VideoRun>>,
}

impl Snapshots {
    pub fn new(source: Source) -> Arc<Snapshots> {
        let hd = match &source {
            Source::Stream(url) => hd_of(url),
            Source::Jpeg(_) => None,
        };
        Arc::new(Snapshots {
            source,
            hd: Mutex::new(hd),
            running: tokio::sync::Mutex::new(None),
            last_asked: Arc::new(Mutex::new(Instant::now())),
            video: tokio::sync::Mutex::new(None),
        })
    }

    /* The newest picture (JPEG); `live`: the stream fully decoded (see
     * the top of the file). */
    pub async fn get(self: &Arc<Self>, live: bool) -> Result<Vec<u8>, String> {
        *self.last_asked.lock().unwrap() = Instant::now();
        match &self.source {
            Source::Jpeg(url) => fetch_jpeg(url).await,
            Source::Stream(url) => self.from_stream(url, live).await,
        }
    }

    async fn from_stream(self: &Arc<Self>, small: &str, live: bool) -> Result<Vec<u8>, String> {
        let hd = if live { None } else { self.hd.lock().unwrap().clone() };
        let url = hd.as_deref().unwrap_or(small);
        let latest = {
            let mut running = self.running.lock().await;
            let fits = match running.as_mut() {
                Some(r) => matches!(r.child.try_wait(), Ok(None)) && r.live == live && r.url == url,
                None => false,
            };
            if !fits {
                let was_running = running.is_some();
                if let Some(mut old) = running.take() {
                    let _ = old.child.start_kill();
                }
                *running = Some(start_ffmpeg(url, live)?);
                if !was_running {
                    self.stop_when_idle();
                }
            }
            running.as_ref().unwrap().latest.clone()
        };
        let deadline = Instant::now() + FIRST_PICTURE;
        loop {
            if let Some((at, picture)) = latest.lock().unwrap().as_ref() {
                if at.elapsed() < FRESH {
                    return Ok(picture.clone());
                }
            }
            if Instant::now() > deadline {
                /* Let the next request start it again. */
                if let Some(mut r) = self.running.lock().await.take() {
                    let _ = r.child.start_kill();
                }
                if hd.is_some() {
                    /* No HD stream after all (or not by that name): stills
                     * from the small stream from now on. */
                    println!("camera: no picture from the HD stream; using the small one");
                    *self.hd.lock().unwrap() = None;
                    return Box::pin(self.from_stream(small, live)).await;
                }
                return Err("no picture from the camera's stream (is the stream account right?)".into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /* Live video at `width` x `height` (the screen's picture size): a
     * receiver of the newest frame. Asking again while it runs at that
     * size just adds a viewer; a stream that broke off starts anew. */
    pub async fn watch_video(self: &Arc<Self>, width: u16, height: u16) -> Result<FrameRx, String> {
        let Source::Stream(url) = &self.source else {
            return Err("this camera gives single pictures, no video".into());
        };
        let size = video_size(width, height);
        let mut video = self.video.lock().await;
        if let Some(run) = video.as_mut() {
            if run.size == size && matches!(run.child.try_wait(), Ok(None)) {
                return Ok(run.frames.subscribe());
            }
        }
        let was_running = video.is_some();
        if let Some(mut old) = video.take() {
            let _ = old.child.start_kill();
        }
        /* One stream from the camera at a time: cameras allow only a few
         * (a Tapo answered "400 Bad Request" to a second one), and the
         * stills aren't needed while the video plays. */
        if let Some(mut stills) = self.running.lock().await.take() {
            let _ = stills.child.start_kill();
        }
        let run = start_video(url, size)?;
        let frames = run.frames.subscribe();
        *video = Some(run);
        if !was_running {
            self.stop_video_when_unwatched();
        }
        Ok(frames)
    }

    /* Stops the video VIDEO_IDLE after its last viewer left. */
    fn stop_video_when_unwatched(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut unwatched_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(me) = me.upgrade() else { return };
                let mut video = me.video.lock().await;
                let Some(run) = video.as_mut() else { return };
                if run.frames.receiver_count() > 0 {
                    unwatched_since = None;
                    continue;
                }
                if unwatched_since.get_or_insert_with(Instant::now).elapsed() >= VIDEO_IDLE {
                    let mut run = video.take().unwrap();
                    let _ = run.child.start_kill();
                    let _ = run.child.wait().await;
                    return;
                }
            }
        });
    }

    /* Stops ffmpeg IDLE after the last request. */
    fn stop_when_idle(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(me) = me.upgrade() else { return };
                if me.last_asked.lock().unwrap().elapsed() >= IDLE {
                    if let Some(mut r) = me.running.lock().await.take() {
                        let _ = r.child.start_kill();
                        let _ = r.child.wait().await;
                    }
                    return;
                }
            }
        });
    }
}

/* The HD stream next to a camera's small one, for the makers whose names
 * are known: Tapo /stream2 -> /stream1, Reolink ..._sub -> ..._main,
 * Hikvision /Streaming/Channels/X02 -> X01, Dahua subtype=1 -> subtype=0.
 * None: not a small stream we recognise (maybe already the HD one). */
pub fn hd_of(url: &str) -> Option<String> {
    if !url.starts_with("rtsp://") {
        return None;
    }
    let (rest, query) = match url.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (url, None),
    };
    if let Some(q) = query {
        return q.contains("subtype=1").then(|| format!("{rest}?{}", q.replace("subtype=1", "subtype=0")));
    }
    if let Some(base) = rest.strip_suffix("/stream2") {
        return Some(format!("{base}/stream1"));
    }
    if let Some(base) = rest.strip_suffix("_sub") {
        return Some(format!("{base}_main"));
    }
    let (base, last) = rest.rsplit_once('/')?;
    if base.ends_with("/Streaming/Channels") && last.len() >= 3 && last.ends_with("02") && last.bytes().all(|b| b.is_ascii_digit()) {
        return Some(format!("{base}/{}01", &last[..last.len() - 2]));
    }
    None
}

/* ffmpeg's arguments for a stream (see the top of the file). Pictures at
 * most 1280 px wide (a stream that small is kept as it is). */
pub fn ffmpeg_args(url: &str, live: bool) -> Vec<String> {
    let mut args: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1"].iter().map(|s| s.to_string()).collect();
    let rtsp = url.starts_with("rtsp://");
    if rtsp {
        /* TCP: no UDP ports to open in the firewall, no lost packets. */
        args.extend(["-rtsp_transport", "tcp"].map(String::from));
        if !live {
            args.extend(["-skip_frame", "nokey"].map(String::from));
        }
    }
    args.extend(["-i", url].map(String::from));
    let size = "scale='min(1280,iw)':-2";
    let filter = match (rtsp, live) {
        (true, false) => size.to_string(),
        /* MJPEG streams are all key frames: still = one a second. */
        (false, false) => format!("fps=1,{size}"),
        (_, true) => format!("fps={LIVE_FPS},{size}"),
    };
    let fps_mode = if live { "cfr" } else { "passthrough" };
    args.extend(["-an", "-sn", "-fps_mode", fps_mode, "-vf", &filter, "-threads", "1", "-c:v", "mjpeg", "-q:v", "3", "-f", "image2pipe", "pipe:1"].map(String::from));
    args
}

fn start_ffmpeg(url: &str, live: bool) -> Result<Running, String> {
    let mut child = Command::new("ffmpeg")
        .args(ffmpeg_args(url, live))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("can't start ffmpeg: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("ffmpeg has no output")?;
    let latest: Latest = Arc::new(Mutex::new(None));
    let sink = latest.clone();
    tokio::spawn(async move {
        let mut splitter = Splitter::default();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stdout.read(&mut buf).await {
            if n == 0 {
                return;
            }
            for picture in splitter.push(&buf[..n]) {
                *sink.lock().unwrap() = Some((Instant::now(), picture));
            }
        }
    });
    Ok(Running { child, latest, live, url: url.to_string() })
}

/* A video size the screen can ask for: within VIDEO_MAX, at least 64 x
 * 36, even numbers (what the pixel conversion wants). */
pub fn video_size(width: u16, height: u16) -> (u16, u16) {
    let even = |v: u16, min: u16, max: u16| v.clamp(min, max) & !1;
    (even(width, 64, VIDEO_MAX.0), even(height, 36, VIDEO_MAX.1))
}

/* ffmpeg's arguments for live video: every frame decoded, scaled to fit
 * width x height (the rest black: the size is exact), raw RGB out. */
pub fn video_args(url: &str, (width, height): (u16, u16)) -> Vec<String> {
    let mut args: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error"].iter().map(|s| s.to_string()).collect();
    /* Two decoding threads: one alone falls just short of real time on
     * the A7 (the measured ~1.1 cores). */
    args.extend(["-threads", "2"].map(String::from));
    if url.starts_with("rtsp://") {
        args.extend(["-rtsp_transport", "tcp"].map(String::from));
    }
    /* Show frames as they come, no buffering; skip H.264's deblocking
     * filter and allow its not-bit-exact shortcuts (-flags2 fast) --
     * both invisible at this size, both cheaper to decode. */
    args.extend(["-fflags", "nobuffer", "-flags", "low_delay", "-flags2", "fast", "-skip_loop_filter", "all", "-i", url].map(String::from));
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease:flags=fast_bilinear,pad={width}:{height}:-1:-1:color=black"
    );
    args.extend(["-an", "-sn", "-fps_mode", "passthrough", "-vf", &filter, "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"].map(String::from));
    args
}

fn start_video(url: &str, size: (u16, u16)) -> Result<VideoRun, String> {
    let mut command = Command::new("ffmpeg");
    command.args(video_args(url, size)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    /* SAFETY: runs in the child between fork and exec; setpriority is
     * async-signal-safe and touches nothing of the parent's. */
    unsafe {
        command.pre_exec(|| {
            /* Not fatal if refused (the video just competes evenly);
             * backend-daemon.service must allow setpriority -- its
             * @resources filter alone refuses it. */
            libc::setpriority(libc::PRIO_PROCESS as _, 0, VIDEO_NICE);
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| format!("can't start ffmpeg: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("ffmpeg has no output")?;
    let (tx, _) = tokio::sync::watch::channel(None);
    let frames = tx.clone();
    tokio::spawn(async move {
        let (width, height) = size;
        let length = width as usize * height as usize * 3;
        /* Frames a second, logged every FPS_LOG: how fast ffmpeg really
         * decodes (below the camera's rate = the A7 can't keep up). */
        let (mut count, mut since) = (0u32, Instant::now());
        loop {
            /* Whole frames only: ffmpeg writes exactly `length` bytes
             * each. Each frame read into its own buffer, which then goes
             * out as it is (a frame is 1.1 MB: copying it costs time). */
            let mut pixels = Vec::with_capacity(length);
            match (&mut stdout).take(length as u64).read_to_end(&mut pixels).await {
                Ok(n) if n == length => {}
                _ => return,
            }
            frames.send_replace(Some(Arc::new(Frame { width, height, pixels })));
            count += 1;
            if since.elapsed() >= FPS_LOG {
                println!("camera.rs: live video {:.1} frames/s from ffmpeg", count as f32 / since.elapsed().as_secs_f32());
                (count, since) = (0, Instant::now());
            }
        }
    });
    Ok(VideoRun { child, size, frames: tx })
}

/* WHICH CAMERA IS WHICH, for the screen's video (ws.rs asks by device
 * id): each camera task registers its Snapshots here. Weak: a removed
 * or restarted camera's entry just stops answering. */
static REGISTRY: std::sync::OnceLock<Mutex<std::collections::HashMap<String, std::sync::Weak<Snapshots>>>> = std::sync::OnceLock::new();

pub fn register(id: &str, snapshots: &Arc<Snapshots>) {
    let mut all = REGISTRY.get_or_init(Default::default).lock().unwrap();
    all.retain(|_, s| s.strong_count() > 0);
    all.insert(id.to_string(), Arc::downgrade(snapshots));
}

/* A camera's pictures by its device id (None: not a camera, or gone). */
pub fn find(id: &str) -> Option<Arc<Snapshots>> {
    REGISTRY.get_or_init(Default::default).lock().unwrap().get(id)?.upgrade()
}

/* Cuts ffmpeg's output into JPEGs: each starts with FF D8 and ends with
 * FF D9 (inside a JPEG's data an FF is always followed by 00, so FF D9
 * only ends a picture). */
#[derive(Default)]
pub struct Splitter {
    buffer: Vec<u8>,
}

impl Splitter {
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let Some(start) = self.buffer.windows(2).position(|w| w == [0xFF, 0xD8]) else {
                self.buffer.clear();
                break;
            };
            let Some(end) = self.buffer[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9]) else {
                /* Not complete yet; drop what's before it. */
                self.buffer.drain(..start);
                if self.buffer.len() > MAX_PICTURE {
                    self.buffer.clear();
                }
                break;
            };
            let end = start + 2 + end + 2;
            out.push(self.buffer[start..end].to_vec());
            self.buffer.drain(..end);
        }
        out
    }
}

/* One JPEG from a URL (an ESP32-CAM's /capture). */
async fn fetch_jpeg(url: &str) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// picture URLs")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let bytes = super::net::http_request(hyper::Method::GET, host, path, None).await.map_err(|e| e.to_string())?;
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Err(format!("{host} didn't answer with a JPEG picture"));
    }
    Ok(bytes.to_vec())
}

/* user:password@ for a URL: percent-encoded, so a password with "@" or
 * ":" doesn't break it. */
pub fn userinfo(user: &str, password: &str) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    if user.is_empty() {
        String::new()
    } else {
        format!("{}:{}@", encode(user), encode(password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_cut_from_the_stream() {
        let mut splitter = Splitter::default();
        let a = [0xFF, 0xD8, 1, 2, 0xFF, 0x00, 3, 0xFF, 0xD9];
        let b = [0xFF, 0xD8, 9, 0xFF, 0xD9];
        /* Arriving in odd pieces. */
        let mut data = a.to_vec();
        data.extend_from_slice(&b);
        assert!(splitter.push(&data[..4]).is_empty());
        let got = splitter.push(&data[4..]);
        assert_eq!(got, vec![a.to_vec(), b.to_vec()]);
    }

    #[test]
    fn ffmpeg_decodes_only_key_frames_of_rtsp() {
        let args = ffmpeg_args("rtsp://u:p@192.168.1.133:554/stream2", false).join(" ");
        assert!(args.contains("-rtsp_transport tcp -skip_frame nokey -i rtsp://u:p@192.168.1.133:554/stream2"), "{args}");
        assert!(args.contains("-threads 1"));
        assert!(args.ends_with("-f image2pipe pipe:1"));
        assert!(ffmpeg_args("http://cam:81/stream", false).join(" ").contains("fps=1,scale="));
        /* Live: every frame decoded, LIVE_FPS out. */
        let live = ffmpeg_args("rtsp://u:p@192.168.1.133:554/stream2", true).join(" ");
        assert!(!live.contains("skip_frame") && live.contains("fps=4,scale="), "{live}");
    }

    #[test]
    fn video_is_raw_rgb_at_the_asked_size() {
        let args = video_args("rtsp://u:p@cam:554/stream2", (480, 270)).join(" ");
        assert!(args.contains("-threads 2 -rtsp_transport tcp"), "{args}");
        assert!(args.contains("-skip_loop_filter all -i rtsp://u:p@cam:554/stream2"), "{args}");
        assert!(args.contains("scale=480:270:force_original_aspect_ratio=decrease"), "{args}");
        assert!(args.contains("pad=480:270:-1:-1"), "{args}");
        assert!(args.ends_with("-pix_fmt rgb24 -f rawvideo pipe:1"), "{args}");
        assert!(!args.contains("skip_frame"), "every frame decoded: {args}");
        assert!(!args.contains("fps="), "every frame shown, at the camera's rate: {args}");
        /* Sizes: even, within the screen. */
        assert_eq!(video_size(481, 271), (480, 270));
        assert_eq!(video_size(4000, 3000), (800, 480));
        assert_eq!(video_size(1, 1), (64, 36));
    }

    #[test]
    fn hd_streams_are_found() {
        let hd = |u: &str| hd_of(u);
        assert_eq!(hd("rtsp://u:p@cam:554/stream2").as_deref(), Some("rtsp://u:p@cam:554/stream1"));
        assert_eq!(hd("rtsp://cam/h264Preview_01_sub").as_deref(), Some("rtsp://cam/h264Preview_01_main"));
        assert_eq!(hd("rtsp://cam/Streaming/Channels/102").as_deref(), Some("rtsp://cam/Streaming/Channels/101"));
        assert_eq!(hd("rtsp://cam/cam/realmonitor?channel=1&subtype=1").as_deref(), Some("rtsp://cam/cam/realmonitor?channel=1&subtype=0"));
        /* Already HD, unknown, or not RTSP. */
        assert_eq!(hd("rtsp://u:p@cam:554/stream1"), None);
        assert_eq!(hd("rtsp://cam/live"), None);
        assert_eq!(hd("http://cam:81/stream"), None);
    }

    #[test]
    fn credentials_are_encoded() {
        assert_eq!(userinfo("cam", "p@ss:w/rd"), "cam:p%40ss%3Aw%2Frd@");
        assert_eq!(userinfo("", "x"), "");
    }
}
