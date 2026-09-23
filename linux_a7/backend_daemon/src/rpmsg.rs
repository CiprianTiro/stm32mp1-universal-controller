/* rpmsg.rs -- the RPMsg link to the M4 firmware (issue #12/#14).
 *
 * This is an actor, following the exact same shape as state.rs's `run()`:
 * it owns the one thing nobody else is allowed to touch directly (here,
 * the open file handle to /dev/rpmsgN), and every other task talks to it
 * only by sending a `Cmd` through a channel and waiting for a reply on a
 * `oneshot` channel bundled inside that message. See state.rs's own doc
 * comments if the actor/mailbox idea itself is unfamiliar -- this file
 * assumes you've already seen that explanation and doesn't repeat it.
 *
 * What's different from state.rs: state.rs's "mailbox" never runs out of
 * things to do wrong (a HashMap lookup can't fail), but every command here
 * involves real I/O against real hardware across a real transport, so most
 * of this file is about what to do when that I/O fails partway through --
 * the M4 not being loaded yet, the connection dying mid-request, etc.
 */

use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/* Must match firmware_m4/src/main.c's RPMSG_RAW_CHANNEL_NAME exactly -- it's
 * also the one specific name Linux's in-tree rpmsg_char kernel driver
 * auto-binds to (drivers/rpmsg/rpmsg_char.c's rpmsg_chrdev_id_table), which
 * is what makes a plain /dev/rpmsgN character device appear for us to open
 * directly below, with no custom kernel module involved. */
const CHANNEL_NAME: &str = "rpmsg-raw";

/* Every message this actor understands. Same idea as state.rs's `Msg` enum:
 * this list is the *complete* set of things anyone is allowed to ask this
 * actor to do. Both variants carry a `oneshot::Sender` "return envelope" --
 * see state.rs's `Msg::GetDevice` doc comment for what that pattern means
 * if it's new -- because unlike state.rs's fire-and-forget `UpdateDevice`,
 * every command here has a real answer the caller needs (what did the LED
 * actually end up as, or why did this fail).
 *
 * The `Ok`/`Err` inside each reply distinguishes "the M4 answered, and here
 * is its answer" (`Ok`) from "we couldn't even talk to the M4 to ask"
 * (`Err(String)` with a human-readable reason) -- ws.rs turns the `Err` case
 * into a `ServerResponse::Error` for whichever client asked. */
pub enum Cmd {
    /// Ask the M4 to turn the LED on or off. Its reply is the LED's
    /// *resulting* state (see firmware_m4/src/main.c's protocol doc comment)
    /// -- normally identical to the `on` value just sent, but this way the
    /// caller never has to just assume the command actually took effect.
    SetLed {
        on: bool,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// Ask the M4 for the LED's current state without changing it -- used
    /// once, right after ui_layer's WebSocket connection opens, so the GUI
    /// can show the real state instead of guessing a default.
    GetLedState { reply: oneshot::Sender<Result<bool, String>> },
}

/* The actor's mailbox loop -- spawned exactly once from main.rs, the same
 * way state::run is. `rx` is the receiving half of the channel; every clone
 * of the matching `Sender<Cmd>` (ws.rs gets one) can enqueue a `Cmd` here,
 * but only this loop ever pulls one out.
 *
 * Unlike the old version of this file (issue #12's ping/ack demo), there's
 * no periodic anything here -- issue #14 removed the M4's heartbeat
 * entirely, so there's nothing to poll on a timer anymore. This loop simply
 * waits for a real command to arrive and only then, if needed, connects. */
pub async fn run(mut rx: mpsc::Receiver<Cmd>) {
    /* The open connection to the M4, if we have one right now. `None` means
     * "not connected yet, or the last attempt failed" -- reconnecting is
     * handled lazily, the next time a command actually needs the link,
     * rather than by a background retry loop running whether anyone's
     * asking or not. */
    let mut link: Option<tokio::fs::File> = None;

    while let Some(cmd) = rx.recv().await {
        /* Turn whichever `Cmd` this is into the one-line text request the
         * M4 firmware's protocol expects (see firmware_m4/src/main.c's
         * protocol doc comment for the exact three strings it understands). */
        let request = match &cmd {
            Cmd::SetLed { on: true, .. } => "LED ON",
            Cmd::SetLed { on: false, .. } => "LED OFF",
            Cmd::GetLedState { .. } => "LED STATUS",
        };

        /* Up to two attempts. Why a retry: when the M4 is restarted (e.g. by
         * `make flash-m4`, which restarts m4-firmware.service), the kernel
         * removes and recreates /dev/rpmsgN. The handle we're holding then
         * points at the OLD, dead channel, and only fails when we next use
         * it ("Broken pipe"). Without a retry the first command after every
         * M4 restart would fail -- the user taps the button and gets an
         * error. So if a handle opened BEFORE this command fails, drop it
         * and try once more with a freshly opened one. A handle opened just
         * now that fails anyway is a real error -- no second retry.
         * Resending is safe: all three requests are idempotent (turning the
         * LED on twice leaves it on). */
        let mut result = Err("M4 firmware not loaded (no rpmsg-raw device)".to_string());
        for _attempt in 0..2 {
            let reused = link.is_some();
            if link.is_none() {
                link = open_device().await;
            }
            let Some(file) = link.as_mut() else {
                /* M4 firmware isn't loaded (yet, or ever, this boot) --
                 * answer honestly rather than leaving the caller waiting. */
                break;
            };

            match round_trip(file, request).await {
                Ok(response) => {
                    result = Ok(response);
                    break;
                }
                Err(e) => {
                    /* Dead connection either way -- drop it so the next
                     * attempt (or next command) opens a fresh one. */
                    link = None;
                    result = Err(format!("rpmsg I/O error: {e}"));
                    if !reused {
                        break;
                    }
                }
            }
        }

        match result {
            Ok(response) => reply_ok(cmd, &response),
            Err(message) => reply_err(cmd, message),
        }
    }
}

/* Sends `request` and reads back exactly one reply, matching the M4
 * firmware's one-message-per-request protocol -- there's no length prefix
 * or delimiter needed because RPMsg already frames each `write()` as one
 * discrete message on the M4 side (see firmware_m4/src/main.c). */
async fn round_trip(file: &mut tokio::fs::File, request: &str) -> std::io::Result<String> {
    file.write_all(request.as_bytes()).await?;

    let mut buf = [0u8; 256];
    let n = file.read(&mut buf).await?;
    if n == 0 {
        /* A zero-byte read on a normal file means EOF; here it means the M4
         * side closed the channel (crashed, or remoteproc stopped it) --
         * treat it the same as any other I/O failure so the caller above
         * drops the stale connection. */
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "M4 closed the rpmsg channel",
        ));
    }

    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/* Turns the M4's raw reply text into what `Cmd`'s reply channel actually
 * expects: both `Cmd` variants want a `bool` (the LED's state), not the raw
 * "LED ON"/"LED OFF" string -- this is the one place that string gets
 * parsed, so a future protocol change only needs to change it here. */
fn reply_ok(cmd: Cmd, response: &str) {
    let on = response.trim() == "LED ON";
    match cmd {
        Cmd::SetLed { reply, .. } => {
            /* `let _ =` discards the Result that `oneshot::Sender::send`
             * returns: it's `Err` only if whoever was waiting for this
             * reply already gave up (e.g. their WebSocket disconnected) --
             * not this actor's problem to handle, same reasoning as
             * state.rs's own `oneshot` sends. */
            let _ = reply.send(Ok(on));
        }
        Cmd::GetLedState { reply } => {
            let _ = reply.send(Ok(on));
        }
    }
}

/// Sends an `Err` back on whichever `Cmd` variant this is -- factored out
/// since both variants carry the same `Result<bool, String>` reply shape,
/// just under different field names.
fn reply_err(cmd: Cmd, message: String) {
    match cmd {
        Cmd::SetLed { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Cmd::GetLedState { reply } => {
            let _ = reply.send(Err(message));
        }
    }
}

/* Finds and opens the M4's rpmsg-raw device, or returns `None` if it's not
 * there right now. The M4 announces its endpoint dynamically once
 * `remoteproc` starts it, so the /dev/rpmsgN *number* isn't fixed -- which
 * one is ours can only be found by reading the `name` file under each
 * /sys/class/rpmsg/rpmsgN/ entry and checking which one says "rpmsg-raw",
 * rather than assuming a number that could point at a different channel (or
 * nothing at all) depending on boot order. */
async fn open_device() -> Option<tokio::fs::File> {
    let path = find_device().await?;

    match tokio::fs::OpenOptions::new().read(true).write(true).open(&path).await {
        Ok(file) => {
            println!("rpmsg: connected to M4 over {}", path.display());
            Some(file)
        }
        Err(e) => {
            println!("rpmsg: failed to open {}: {e}", path.display());
            None
        }
    }
}

async fn find_device() -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir("/sys/class/rpmsg").await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        /* Not every entry here has a `name` file -- rpmsg_ctrl0 doesn't,
         * since it's the bus-level control device, not a channel. Skip just
         * that one entry on a read error instead of aborting the whole
         * scan, or a control device sorted before ours would hide a channel
         * that's actually there (this bit us once already -- see the
         * GitHub wiki's "M4-Firmware" page). */
        let Ok(name) = tokio::fs::read_to_string(entry.path().join("name")).await else {
            continue;
        };
        if name.trim() == CHANNEL_NAME {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}
