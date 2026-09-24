/*
 * store.rs -- small files that survive power cuts and detect corruption
 * (issue #33). Used for the device registry (state.rs) and the list of
 * shadows that exist in the cloud (mqtt.rs).
 *
 * THE FILE FORMAT. One header line, then the data (the "payload", JSON):
 *
 *     UCSTORE schema=1 len=57 crc32=8a3c19f0
 *     {"lamp-1":{"on":true}, ...}
 *
 *   - schema: which layout the payload has. When the layout changes later
 *     (e.g. the capability model), the number goes up and the loader can
 *     still read -- and convert -- files written by an older daemon.
 *   - len: the payload's exact length in bytes. A file cut short (power
 *     lost mid-write) is caught by this even before the checksum.
 *   - crc32: a checksum of the payload bytes. Change even one bit of the
 *     payload and the checksum no longer matches. It catches ACCIDENTAL
 *     damage (a torn write, a bad flash cell); it does NOT protect against
 *     someone deliberately editing the file -- they can simply recompute
 *     it. Stopping that needs a signature (see the wiki's Security-Plan).
 *
 * THREE FILES PER STORE, e.g. for "devices.json":
 *   devices.json       the latest copy
 *   devices.json.prev  the copy before it (the fallback)
 *   devices.json.tmp   only exists briefly while a new copy is written
 * plus devices.json.bad / devices.json.prev.bad: a copy found damaged at
 * start, moved aside (never silently deleted) so it can be inspected.
 *
 * HOW A SAVE STAYS SAFE AGAINST POWER LOSS (see Store::save):
 *   1. write the new copy to .tmp and fsync it (it's now really on flash)
 *   2. rename the latest copy to .prev
 *   3. rename .tmp to the latest copy
 *   4. fsync the directory (makes the two renames themselves permanent)
 * A rename replaces a file in one step: anyone looking sees either the old
 * file or the new one, never a mix. So wherever the power goes, at least
 * one complete, checksummed copy exists -- a cut during 1 leaves a broken
 * .tmp (ignored), a cut between 2 and 3 leaves no latest copy but a good
 * .prev (loaded instead).
 *
 * Everything here is plain blocking file I/O (std::fs, not tokio::fs):
 * fsync can take tens of milliseconds on flash, so `writer` below runs it
 * on Tokio's separate thread pool for blocking work (spawn_blocking),
 * never on the async worker threads that serve everything else.
 */
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;

/* Where the stores live unless overridden: the per-board userfs partition,
 * next to the board's config in /usr/local/etc (see mqtt.rs). /var/lib
 * is the usual Linux place for "a program's own data", under /usr/local
 * because that partition survives rootfs updates (#16) and stays writable
 * once the rootfs is read-only (#18). */
const DEFAULT_DATA_DIR: &str = "/usr/local/var/lib/universal-controller";

/* The first word of every file, so a completely different file that
 * happens to sit at this path is never mistaken for one of ours. */
const MAGIC: &str = "UCSTORE";

/* The data directory: $HUB_DATA_DIR if set (handy when running the daemon
 * on the dev PC, where /usr/local isn't ours to write), else the default. */
pub fn data_dir() -> PathBuf {
    std::env::var_os("HUB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
}

/* CRC-32 (the common "IEEE" variant, the same one zip and Ethernet use),
 * computed bit by bit. Faster versions use a 256-entry lookup table, but
 * our files are a few kilobytes and written only when something changes,
 * so the simplest correct version is plenty -- and needs no extra crate.
 *
 * The idea: treat the data as one huge binary number and divide it by a
 * fixed 33-bit number (the "polynomial", 0xEDB88320 in this bit-reversed
 * form); the remainder is the checksum. Any small change to the data
 * changes the remainder. The ! at the start and end is part of the
 * standard definition (so leading zero bytes still count). */
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            /* Shift one bit out; if it was a 1, "subtract" (XOR) the
             * polynomial. `0u32.wrapping_sub(crc & 1)` is all-ones when the
             * low bit is 1 and zero otherwise -- a branch-free if. */
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/* Header + payload, as written to disk. */
pub fn encode(schema: u32, payload: &[u8]) -> Vec<u8> {
    let header = format!("{MAGIC} schema={schema} len={} crc32={:08x}\n", payload.len(), crc32(payload));
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(payload);
    bytes
}

/* The reverse of encode: checks the header, the length and the checksum,
 * and hands back the schema and the payload (a slice INTO `bytes`, no
 * copy). Err describes what's wrong, for the log. */
pub fn decode(bytes: &[u8]) -> Result<(u32, &[u8]), String> {
    /* The header ends at the first newline. */
    let newline = bytes
        .iter()
        .position(|&b| b == b'\n')
        .ok_or("no header line (file empty or cut short)")?;
    let header = std::str::from_utf8(&bytes[..newline]).map_err(|_| "header is not text")?;
    let payload = &bytes[newline + 1..];

    /* Exactly four words, in this order; anything else is damage. */
    let words: Vec<&str> = header.split(' ').collect();
    let [magic, schema, len, crc] = words.as_slice() else {
        return Err(format!("malformed header \"{header}\""));
    };
    if *magic != MAGIC {
        return Err(format!("not a store file (header \"{header}\")"));
    }
    /* `field("schema=", "schema=1")` -> Some("1"). */
    let field = |name: &str, word: &'_ str| word.strip_prefix(name).map(str::to_owned);
    let bad = || format!("malformed header \"{header}\"");
    let schema: u32 = field("schema=", schema).and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let len: usize = field("len=", len).and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let crc = field("crc32=", crc)
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
        .ok_or_else(bad)?;

    if payload.len() != len {
        return Err(format!("length mismatch: header says {len} bytes, file has {} (write interrupted?)", payload.len()));
    }
    let actual = crc32(payload);
    if actual != crc {
        return Err(format!("checksum mismatch: header says {crc:08x}, data gives {actual:08x} (data corrupted)"));
    }
    Ok((schema, payload))
}

/* What load() found. Generic over T, the decoded value (e.g. the device
 * map), so the same logic serves every store. */
#[derive(Debug, PartialEq)]
pub enum Load<T> {
    /* Neither copy exists: the very first start. Not an error. */
    Fresh,
    /* `problem` is None when the latest copy was fine; Some(what went
     * wrong with it) when the previous copy had to be used instead. */
    Loaded { value: T, problem: Option<String> },
    /* Both copies are damaged (kept as *.bad). */
    Failed(String),
}

/* One store = one file name in the data directory (with its .prev/.tmp
 * companions). Holds only paths; opening a file happens per call. */
pub struct Store {
    latest: PathBuf,
    prev: PathBuf,
    tmp: PathBuf,
}

/* One copy on disk, looked at by read_copy. */
enum Copy<T> {
    Missing,
    Good(T),
    Bad(String),
}

impl Store {
    pub fn new(dir: &Path, name: &str) -> Self {
        Store {
            latest: dir.join(name),
            prev: dir.join(format!("{name}.prev")),
            tmp: dir.join(format!("{name}.tmp")),
        }
    }

    /* The file's name, for log messages. */
    pub fn name(&self) -> String {
        self.latest.display().to_string()
    }

    /* Reads the latest copy, falling back to the previous one.
     *
     * `decode_payload` turns (schema, payload) into the value, or says why
     * it can't (unknown schema, invalid JSON). A copy only counts as good
     * if BOTH its checksum and decode_payload are fine -- so the fallback
     * also covers "checksum right but content unusable".
     *
     * A damaged copy is renamed to *.bad right here: kept for inspection,
     * and out of the way so the next save can't rotate it into .prev over
     * a good copy. */
    pub fn load<T>(&self, decode_payload: impl Fn(u32, &[u8]) -> Result<T, String>) -> Load<T> {
        let latest_problem = match read_copy(&self.latest, &decode_payload) {
            Copy::Good(value) => return Load::Loaded { value, problem: None },
            Copy::Missing => None,
            Copy::Bad(problem) => {
                self.quarantine(&self.latest);
                Some(problem)
            }
        };
        match (read_copy(&self.prev, &decode_payload), latest_problem) {
            (Copy::Missing, None) => Load::Fresh,
            (Copy::Missing, Some(problem)) => Load::Failed(problem),
            (Copy::Good(value), problem) => Load::Loaded {
                value,
                /* No latest copy but a good .prev: power was lost between
                 * the two renames of a save. */
                problem: Some(problem.unwrap_or_else(|| "latest copy missing (write interrupted?)".into())),
            },
            (Copy::Bad(prev_problem), problem) => {
                self.quarantine(&self.prev);
                Load::Failed(match problem {
                    Some(problem) => format!("{problem}; previous copy: {prev_problem}"),
                    None => format!("latest copy missing; previous copy: {prev_problem}"),
                })
            }
        }
    }

    /* load(), with every outcome logged, and T's default (e.g. an empty
     * map) when there's nothing usable. `what` names the content for the
     * log ("device registry"). */
    pub fn load_or_default<T: Default>(&self, what: &str, decode_payload: impl Fn(u32, &[u8]) -> Result<T, String>) -> T {
        match self.load(decode_payload) {
            Load::Fresh => {
                println!("store: no {what} yet ({}), starting empty", self.name());
                T::default()
            }
            Load::Loaded { value, problem: None } => value,
            Load::Loaded { value, problem: Some(problem) } => {
                println!("store: WARNING {what}: latest copy unusable ({problem}) -- using the PREVIOUS copy, the last change may be lost");
                value
            }
            Load::Failed(problem) => {
                println!("store: ERROR {what}: no usable copy ({problem}) -- starting EMPTY; damaged files kept as *.bad in {}", self.parent().display());
                T::default()
            }
        }
    }

    /* Writes a new copy -- see the header comment for why each step is
     * there. Blocking; call it via `writer` (or spawn_blocking). */
    pub fn save(&self, schema: u32, payload: &[u8]) -> io::Result<()> {
        let dir = self.parent();
        /* mode 0700 / 0600: only the daemon's user (root) can read these.
         * Device entries will later hold things like camera passwords. The
         * modes only apply when the directory/file is CREATED. */
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

        /* 1. Complete new copy in .tmp, forced onto the flash. The inner
         * block closes the file (drops it) before it's renamed. */
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.tmp)?;
            file.write_all(&encode(schema, payload))?;
            file.sync_all()?;
        }
        /* 2. Keep the current copy as the fallback -- but only if it's
         * intact. A copy that went bad since start must not replace a good
         * .prev; it's simply overwritten in step 3. */
        if fs::read(&self.latest).is_ok_and(|bytes| decode(&bytes).is_ok()) {
            fs::rename(&self.latest, &self.prev)?;
        }
        /* 3. The new copy becomes the latest. */
        fs::rename(&self.tmp, &self.latest)?;
        /* 4. A rename changes the DIRECTORY (its list of names), so the
         * directory has to be fsynced too, or the renames themselves could
         * be lost at power-off. On Linux a directory is opened read-only
         * and synced like a file. */
        File::open(dir)?.sync_all()
    }

    fn parent(&self) -> &Path {
        self.latest.parent().unwrap_or(Path::new("."))
    }

    /* Moves a damaged copy to <name>.bad (replacing an older .bad). Only
     * logged if it fails: the damage itself is reported by the caller. */
    fn quarantine(&self, path: &Path) {
        let mut bad = path.as_os_str().to_owned();
        bad.push(".bad");
        if let Err(e) = fs::rename(path, &bad) {
            println!("store: could not move damaged {} aside: {e}", path.display());
        }
    }
}

/* Reads and checks one copy. Only "file not found" counts as Missing; any
 * other read error (permissions, I/O error) is a Bad copy. */
fn read_copy<T>(path: &Path, decode_payload: &impl Fn(u32, &[u8]) -> Result<T, String>) -> Copy<T> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Copy::Missing,
        Err(e) => return Copy::Bad(format!("{}: {e}", path.display())),
    };
    match decode(&bytes).and_then(|(schema, payload)| decode_payload(schema, payload)) {
        Ok(value) => Copy::Good(value),
        Err(problem) => Copy::Bad(format!("{}: {problem}", path.display())),
    }
}

/* Starts a background task that saves whatever payload is sent to the
 * returned channel. Callers just `send_replace(bytes)` and never wait for
 * the flash.
 *
 * Why a `watch` channel: it holds only the LATEST value. If three changes
 * arrive while a save is still running, the task wakes once afterwards and
 * saves only the newest state -- the two in between were already outdated.
 * Fewer writes, less flash wear, and a save can never be overtaken by an
 * older one, because there's only one task doing them, one at a time.
 *
 * Must be called from inside the Tokio runtime (tokio::spawn). On
 * shutdown, a save already running is finished (Tokio waits for
 * spawn_blocking work when the runtime stops). */
pub fn writer(store: Store, schema: u32) -> watch::Sender<Vec<u8>> {
    /* The initial empty value counts as "already seen", so changed() only
     * fires for real sends. */
    let (tx, mut rx) = watch::channel(Vec::new());
    let store = Arc::new(store);
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            /* borrow_and_update: read the value AND mark it seen. Cloned
             * out right away -- the borrow holds the channel's lock. */
            let payload = rx.borrow_and_update().clone();
            let task_store = store.clone();
            match tokio::task::spawn_blocking(move || task_store.save(schema, &payload)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => println!("store: could not save {}: {e}", store.name()),
                Err(e) => println!("store: saving {} crashed: {e}", store.name()),
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A fresh, empty directory per test (tests run in parallel, so each
     * needs its own), under the system's temp dir. */
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uc-store-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /* The decoder the tests use: payload must be UTF-8 text, schema 1. */
    fn text(schema: u32, payload: &[u8]) -> Result<String, String> {
        if schema != 1 {
            return Err(format!("unsupported schema {schema}"));
        }
        String::from_utf8(payload.to_vec()).map_err(|e| e.to_string())
    }

    fn loaded(value: &str) -> Load<String> {
        Load::Loaded { value: value.into(), problem: None }
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        /* Every CRC-32 (IEEE) implementation gives cbf43926 for "123456789"
         * -- the published check value for this exact algorithm. */
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn encode_decode_round_trip() {
        let bytes = encode(3, b"{\"a\":1}");
        assert!(bytes.starts_with(b"UCSTORE schema=3 len=7 crc32="));
        assert_eq!(decode(&bytes), Ok((3, &b"{\"a\":1}"[..])));
    }

    #[test]
    fn decode_detects_damage() {
        let good = encode(1, b"hello world");

        /* One flipped bit in the payload. */
        let mut flipped = good.clone();
        *flipped.last_mut().unwrap() ^= 0x01;
        assert!(decode(&flipped).unwrap_err().contains("checksum mismatch"));

        /* Cut short, as by a power cut mid-write. */
        assert!(decode(&good[..good.len() - 3]).unwrap_err().contains("length mismatch"));
        assert!(decode(&good[..10]).is_err());
        assert!(decode(b"").is_err());

        /* Header damage. */
        assert!(decode(b"OTHER schema=1 len=0 crc32=00000000\n").is_err());
        assert!(decode(b"UCSTORE schema=x len=0 crc32=00000000\n").is_err());
        assert!(decode(b"UCSTORE schema=1 len=0\n").is_err());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = test_dir("round-trip");
        let store = Store::new(&dir, "data.json");
        assert_eq!(store.load(text), Load::Fresh);

        store.save(1, b"first").unwrap();
        assert_eq!(store.load(text), loaded("first"));

        /* The second save keeps the first as .prev, and leaves no .tmp. */
        store.save(1, b"second").unwrap();
        assert_eq!(store.load(text), loaded("second"));
        assert_eq!(decode(&fs::read(dir.join("data.json.prev")).unwrap()), Ok((1, &b"first"[..])));
        assert!(!dir.join("data.json.tmp").exists());
    }

    #[test]
    fn corrupted_latest_falls_back_to_previous_and_is_kept_aside() {
        let dir = test_dir("corrupt");
        let store = Store::new(&dir, "data.json");
        store.save(1, b"old").unwrap();
        store.save(1, b"new").unwrap();

        /* A flash bit flip in the latest copy. */
        let mut bytes = fs::read(dir.join("data.json")).unwrap();
        *bytes.last_mut().unwrap() ^= 0x10;
        fs::write(dir.join("data.json"), &bytes).unwrap();

        match store.load(text) {
            Load::Loaded { value, problem: Some(problem) } => {
                assert_eq!(value, "old");
                assert!(problem.contains("checksum mismatch"), "{problem}");
            }
            other => panic!("expected fallback, got {other:?}"),
        }
        /* Moved aside, byte for byte, not deleted. */
        assert_eq!(fs::read(dir.join("data.json.bad")).unwrap(), bytes);
        assert!(!dir.join("data.json").exists());

        /* The next save must not rotate anything over the good .prev. */
        store.save(1, b"newer").unwrap();
        assert_eq!(store.load(text), loaded("newer"));
        assert_eq!(decode(&fs::read(dir.join("data.json.prev")).unwrap()), Ok((1, &b"old"[..])));
    }

    #[test]
    fn interrupted_writes_never_lose_the_last_good_copy() {
        let dir = test_dir("interrupted");
        let store = Store::new(&dir, "data.json");
        store.save(1, b"old").unwrap();
        store.save(1, b"good").unwrap();

        /* Power lost during step 1: a half-written .tmp is simply ignored. */
        let partial = encode(1, b"never finished");
        fs::write(dir.join("data.json.tmp"), &partial[..partial.len() / 2]).unwrap();
        assert_eq!(store.load(text), loaded("good"));

        /* Power lost between steps 2 and 3: latest already renamed to .prev,
         * .tmp not yet renamed in. */
        fs::rename(dir.join("data.json"), dir.join("data.json.prev")).unwrap();
        match store.load(text) {
            Load::Loaded { value, problem: Some(problem) } => {
                assert_eq!(value, "good");
                assert!(problem.contains("missing"), "{problem}");
            }
            other => panic!("expected fallback, got {other:?}"),
        }

        /* And the next save cleans up the leftover .tmp. */
        store.save(1, b"after").unwrap();
        assert_eq!(store.load(text), loaded("after"));
        assert!(!dir.join("data.json.tmp").exists());
    }

    #[test]
    fn both_copies_damaged_is_a_clear_failure() {
        let dir = test_dir("both-bad");
        let store = Store::new(&dir, "data.json");
        fs::write(dir.join("data.json"), b"garbage").unwrap();
        fs::write(dir.join("data.json.prev"), b"more garbage").unwrap();

        assert!(matches!(store.load(text), Load::Failed(_)));
        assert!(dir.join("data.json.bad").exists());
        assert!(dir.join("data.json.prev.bad").exists());
        /* load_or_default hands back the empty default. */
        assert_eq!(store.load_or_default("test data", text), String::new());
    }

    #[test]
    fn content_the_decoder_rejects_counts_as_damaged() {
        /* Checksum fine, but written by a newer daemon (schema 2) this one
         * can't read: fall back rather than misread it. */
        let dir = test_dir("schema");
        let store = Store::new(&dir, "data.json");
        store.save(1, b"v1 data").unwrap();
        store.save(2, b"v2 data").unwrap();
        match store.load(text) {
            Load::Loaded { value, problem: Some(problem) } => {
                assert_eq!(value, "v1 data");
                assert!(problem.contains("unsupported schema 2"), "{problem}");
            }
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn writer_saves_the_latest_value() {
        let dir = test_dir("writer");
        let tx = writer(Store::new(&dir, "data.json"), 1);
        tx.send_replace(b"one".to_vec());
        tx.send_replace(b"two".to_vec());

        /* The save runs in the background: poll until it lands (at most
         * ~2 s, so a broken writer fails the test instead of hanging). */
        let store = Store::new(&dir, "data.json");
        for _ in 0..200 {
            if store.load(text) == loaded("two") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("writer never saved the latest value");
    }
}
