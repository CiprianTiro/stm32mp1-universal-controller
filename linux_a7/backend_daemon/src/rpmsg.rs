use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/* Must match firmware_m4/src/main.c's RPMSG_RAW_CHANNEL_NAME - also the one
 * name Linux's in-tree rpmsg_char driver auto-binds to
 * (drivers/rpmsg/rpmsg_char.c's rpmsg_chrdev_id_table), which is what makes
 * a plain /dev/rpmsgN show up for us to open directly, no custom kernel
 * module needed. */
const CHANNEL_NAME: &str = "rpmsg-raw";
const PING_PERIOD: Duration = Duration::from_secs(10);
const RETRY_PERIOD: Duration = Duration::from_secs(5);

/* Protocol v0 (see firmware_m4/src/main.c's file header for the M4 side):
 * we send an arbitrary text request, the M4 replies "ACK <n>: <request>".
 * Deliberately minimal - not meant to survive Sprint 3, just proves the
 * link works end to end (issue #12's DoD) before any real framing exists. */
pub async fn run() {
    loop {
        match find_device().await {
            Some(path) => run_link(&path).await,
            None => println!("rpmsg: no {CHANNEL_NAME} device yet (M4 firmware not loaded?)"),
        }
        tokio::time::sleep(RETRY_PERIOD).await;
    }
}

/* The M4 announces its endpoint dynamically once remoteproc starts it, so
 * the /dev/rpmsgN number isn't fixed - the `name` file under each
 * /sys/class/rpmsg/rpmsgN/ entry is how we find which one is ours instead
 * of hardcoding a number that could point at a different channel (or
 * nothing) depending on boot order. */
async fn find_device() -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir("/sys/class/rpmsg").await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        /* Not every entry here has a `name` file -- rpmsg_ctrl0 doesn't,
         * since it's the bus-level control device, not a channel. Skip
         * just that one entry on a read error instead of aborting the
         * whole scan, or a control device sorted before ours would hide
         * a channel that's actually there. */
        let Ok(name) = tokio::fs::read_to_string(entry.path().join("name")).await else {
            continue;
        };
        if name.trim() == CHANNEL_NAME {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

async fn run_link(path: &PathBuf) {
    let mut file = match tokio::fs::OpenOptions::new().read(true).write(true).open(path).await {
        Ok(f) => f,
        Err(e) => {
            println!("rpmsg: failed to open {}: {e}", path.display());
            return;
        }
    };
    println!("rpmsg: connected to M4 over {}", path.display());

    let mut tick = tokio::time::interval(PING_PERIOD);
    let mut buf = [0u8; 256];
    let mut counter = 0u32;

    loop {
        tick.tick().await;
        counter += 1;
        let request = format!("ping {counter}");

        if let Err(e) = file.write_all(request.as_bytes()).await {
            println!("rpmsg: write failed: {e}, will reopen");
            return;
        }

        match file.read(&mut buf).await {
            Ok(0) => {
                println!("rpmsg: M4 closed the channel, will reopen");
                return;
            }
            Ok(n) => {
                let response = String::from_utf8_lossy(&buf[..n]);
                println!("rpmsg: sent {request:?}, got {response:?}");
            }
            Err(e) => {
                println!("rpmsg: read failed: {e}, will reopen");
                return;
            }
        }
    }
}
