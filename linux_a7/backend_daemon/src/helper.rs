/*
 * helper.rs -- asks hub-helper, a tiny root service, to do the few things
 * backend_daemon itself is no longer allowed to do (issue #37).
 *
 * WHY. backend_daemon runs as the unprivileged user "hubd", so that a bug
 * in it -- it parses everything the network sends -- can't be turned into
 * control over the whole board. Almost everything it does works without
 * root (see backend-daemon.service for how). Four things don't:
 *
 *   hotspot-start / hotspot-stop   start/stop hub-hotspot.service (#36)
 *   avahi-restart                  restart avahi after its config changed
 *   wifi-sync                      fsync wpa_supplicant's saved config,
 *                                  a root-only file (it holds passwords)
 *
 * Starting services is root's business in systemd (a normal user would
 * need polkit, a whole extra daemon). Instead, hub-helper listens on the
 * socket below, which only the hubd group may open, and knows exactly these
 * four fixed words -- no arguments, nothing to inject into. So even a fully
 * taken-over backend_daemon can do no more than open or close the hotspot,
 * restart avahi, or flush one file. See hub-helper.sh in the
 * backend-daemon recipe for the other side.
 *
 * The protocol: connect, send the command and a newline, read one line
 * back: "OK" or "ERR <what went wrong>". One command per connection.
 */
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/* Created by hub-helper.socket (systemd listens on it for us). */
const SOCKET: &str = "/run/hub-helper.sock";

/* Starting the hotspot takes the longest (creating uap0, starting hostapd):
 * a few seconds. Anything past this means the helper is stuck. */
const TIMEOUT: Duration = Duration::from_secs(60);

/* The commands hub-helper knows. An enum rather than a free string, so a
 * typo is a compile error here instead of an "unknown command" on the
 * board. */
#[derive(Clone, Copy, Debug)]
pub enum Command {
    HotspotStart,
    HotspotStop,
    AvahiRestart,
    WifiSync,
}

impl Command {
    fn word(self) -> &'static str {
        match self {
            Command::HotspotStart => "hotspot-start",
            Command::HotspotStop => "hotspot-stop",
            Command::AvahiRestart => "avahi-restart",
            Command::WifiSync => "wifi-sync",
        }
    }
}

/* Runs one command and waits for its result. */
pub async fn run(command: Command) -> Result<(), String> {
    let word = command.word();
    let answer = tokio::time::timeout(TIMEOUT, exchange(word))
        .await
        .map_err(|_| format!("hub-helper: no answer to {word} in time"))??;
    parse_answer(word, &answer)
}

async fn exchange(word: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(SOCKET)
        .await
        .map_err(|e| format!("hub-helper not reachable ({SOCKET}): {e}"))?;
    stream
        .write_all(format!("{word}\n").as_bytes())
        .await
        .map_err(|e| format!("hub-helper: {e}"))?;
    /* Tell the helper nothing more is coming, then read everything it
     * says until it closes the connection (it exits after one command).
     * `take` caps the answer at 4 KiB: the helper only ever sends one
     * short line, and a bug there must not make us buffer without end. */
    stream.shutdown().await.map_err(|e| format!("hub-helper: {e}"))?;
    let mut answer = String::new();
    stream
        .take(4096)
        .read_to_string(&mut answer)
        .await
        .map_err(|e| format!("hub-helper: {e}"))?;
    Ok(answer)
}

fn parse_answer(word: &str, answer: &str) -> Result<(), String> {
    let line = answer.lines().next().unwrap_or("").trim();
    if line == "OK" {
        Ok(())
    } else if let Some(reason) = line.strip_prefix("ERR") {
        Err(format!("{word} failed: {}", reason.trim()))
    } else {
        Err(format!("{word}: unexpected answer from hub-helper: {line:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer("wifi-sync", "OK\n"), Ok(()));
        assert_eq!(
            parse_answer("hotspot-start", "ERR Job failed\n"),
            Err("hotspot-start failed: Job failed".into())
        );
        /* The helper crashed or said nothing at all. */
        assert!(parse_answer("avahi-restart", "").is_err());
    }
}
