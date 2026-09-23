/* health.rs -- the hub's own health numbers, for the cloud (issue #29).
 *
 * Builds the `system` section of the Device Shadow report:
 *
 *   "system": {
 *     "cpu_percent": 7.5,            both cores together, since last report
 *     "memory": {"used_percent": 31.2, "available_mb": 290},
 *     "temperature_c": 48.3,         the SoC's own sensor
 *     "disk": {"rootfs_used_percent": 4.0, "userfs_used_percent": 0.1},
 *     "uptime_s": 5231,              seconds since boot
 *     "device_count": 1,             devices the hub manages (incl. ld7)
 *     "local_clients": 1,            WebSocket clients right now (UI, phones)
 *     "m4": "running",               the Cortex-M4's remoteproc state
 *     "version": "0.1.0"             this daemon's version
 *   }
 *
 * Everything comes from files the Linux kernel provides (/proc, /sys) or one
 * libc call (statvfs) -- no external tools like `top` or `df`, which would
 * mean starting a whole process every 10 s.
 *
 * Any value that can't be read (e.g. there's no thermal sensor or M4 in
 * QEMU) becomes `null` in the JSON. A missing number must never stop the
 * report -- the device states in the same message matter more.
 *
 * The parsing is done by small functions that take the file's TEXT rather
 * than a path, so the unit tests at the bottom can feed them fixed sample
 * contents instead of needing a real board.
 */

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/* Where the numbers come from on the DK2 (checked on the board). */
const PROC_STAT: &str = "/proc/stat";
const PROC_MEMINFO: &str = "/proc/meminfo";
const PROC_UPTIME: &str = "/proc/uptime";
/* The STM32MP157's only thermal zone is the CPU die sensor ("cpu-thermal"). */
const CPU_TEMP: &str = "/sys/class/thermal/thermal_zone0/temp";
/* remoteproc0 is the Cortex-M4 (its `name` file says "m4"). */
const M4_STATE: &str = "/sys/class/remoteproc/remoteproc0/state";
/* The two filesystems worth watching: the image (grown to 3.8 GB on first
 * boot) and the per-device userfs partition (certificates, config). */
const ROOTFS: &str = "/";
const USERFS: &str = "/usr/local";

/* CPU time counters from the first ("cpu ") line of /proc/stat, in "ticks"
 * (1/100 s on this kernel), counted since boot -- they only ever grow. */
#[derive(Clone, Copy, Debug, PartialEq)]
struct CpuTimes {
    /* ticks spent doing nothing (idle + waiting for I/O) */
    idle: u64,
    /* ticks spent in total, busy or not */
    total: u64,
}

/* Keeps what's needed between two reports. Only one thing: the previous CPU
 * counters -- see cpu_percent() for why a single reading isn't enough. */
pub struct Sampler {
    prev_cpu: Option<CpuTimes>,
    /* Shared with ws.rs, which counts its connected clients in it. Arc +
     * AtomicUsize = a number several tasks can read and change safely
     * without a lock (same idea as mqtt.rs's `connected` flag). */
    local_clients: Arc<AtomicUsize>,
}

impl Sampler {
    pub fn new(local_clients: Arc<AtomicUsize>) -> Self {
        Sampler {
            prev_cpu: None,
            local_clients,
        }
    }

    /* Reads everything and returns the `system` JSON object.
     * `device_count` comes from the caller, which has the device list.
     *
     * Note: these are ordinary blocking file reads inside async code. That's
     * fine here: /proc and /sys aren't on a disk -- the kernel generates
     * their contents on the spot, so each read takes microseconds. */
    pub fn sample(&mut self, device_count: usize) -> Value {
        let cpu_now = read(PROC_STAT).and_then(|s| parse_cpu_times(&s));
        let cpu_percent = match (self.prev_cpu, cpu_now) {
            (Some(prev), Some(now)) => cpu_percent(prev, now),
            _ => None, /* first report after start: nothing to compare to yet */
        };
        self.prev_cpu = cpu_now;

        let memory = read(PROC_MEMINFO)
            .and_then(|s| parse_meminfo(&s))
            .map(|m| {
                json!({
                    "used_percent": round1(100.0 * (m.total_kb - m.available_kb) as f64 / m.total_kb as f64),
                    "available_mb": m.available_kb / 1024,
                })
            });

        json!({
            "cpu_percent": cpu_percent,
            "memory": memory,
            "temperature_c": read(CPU_TEMP).and_then(|s| parse_millidegrees(&s)),
            "disk": {
                "rootfs_used_percent": disk_used_percent(ROOTFS),
                "userfs_used_percent": disk_used_percent(USERFS),
            },
            "uptime_s": read(PROC_UPTIME).and_then(|s| parse_uptime(&s)),
            "device_count": device_count,
            "local_clients": self.local_clients.load(Ordering::Relaxed),
            "m4": read(M4_STATE).map(|s| s.trim().to_string()),
            /* env!() is filled in at COMPILE time from Cargo.toml's
             * `version`, so the binary always knows which version it is. */
            "version": env!("CARGO_PKG_VERSION"),
        })
    }
}

/* Reads a whole (small) file as text; None if it doesn't exist or can't be
 * read -- which then shows up as null in the report. */
fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/* Rounds to one decimal: 31.2468 -> 31.2. Nobody needs more precision in a
 * dashboard, and it keeps the JSON short. */
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/* Parses the first line of /proc/stat, which looks like:
 *   cpu  1554 4 2134 42699 147 0 14 0 0 0
 * The columns are ticks spent in: user nice system idle iowait irq softirq
 * steal guest guest_nice. The last two are already included in "user"/
 * "nice", so only the first 8 are added up (counting them twice would make
 * the CPU look busier than it is). */
fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1) /* the "cpu" label itself */
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 5 {
        return None;
    }
    Some(CpuTimes {
        idle: fields[3] + fields[4], /* idle + iowait */
        total: fields.iter().sum(),
    })
}

/* CPU usage between two readings of /proc/stat.
 *
 * Why two readings: the counters are totals since boot. One reading can
 * only say "busy X% of the time since boot", which barely moves after a
 * few hours. What we want is "how busy since the last report", so we take
 * the DIFFERENCE between two readings:
 *   busy% = 100 * (1 - idle ticks in between / all ticks in between)
 *
 * None if no time passed at all (two reads in the same tick: dividing by
 * zero), or if the counters went backwards (can't happen normally). */
fn cpu_percent(prev: CpuTimes, now: CpuTimes) -> Option<f64> {
    let total = now.total.checked_sub(prev.total)?;
    let idle = now.idle.checked_sub(prev.idle)?;
    if total == 0 {
        return None;
    }
    Some(round1(100.0 * (1.0 - idle as f64 / total as f64)))
}

struct MemInfo {
    total_kb: u64,
    available_kb: u64,
}

/* Picks two lines out of /proc/meminfo:
 *   MemTotal:         395832 kB
 *   MemAvailable:     325500 kB
 * MemAvailable, not MemFree: Linux uses "free" RAM as a disk cache and gives
 * it back as soon as a program needs it. MemFree ignores that and looks
 * alarmingly low; MemAvailable is what programs can really still get. */
fn parse_meminfo(meminfo: &str) -> Option<MemInfo> {
    let field = |name: &str| -> Option<u64> {
        let line = meminfo.lines().find(|l| l.starts_with(name))?;
        line.split_whitespace().nth(1)?.parse().ok()
    };
    let total_kb = field("MemTotal:")?;
    let available_kb = field("MemAvailable:")?;
    if total_kb == 0 {
        return None;
    }
    Some(MemInfo {
        total_kb,
        available_kb,
    })
}

/* The thermal file holds thousandths of a degree Celsius: "45538" = 45.5 °C. */
fn parse_millidegrees(text: &str) -> Option<f64> {
    let milli: i64 = text.trim().parse().ok()?;
    Some(round1(milli as f64 / 1000.0))
}

/* /proc/uptime is "234.14 427.00": seconds since boot, then idle seconds
 * summed over all cores. Only the first matters. */
fn parse_uptime(text: &str) -> Option<u64> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some(secs as u64)
}

/* How full a filesystem is, the way `df` computes "Use%":
 *   used / (used + available to normal users)
 * ext4 keeps ~5% of blocks reserved for root, which is why `df`'s Used and
 * Avail don't add up to Size -- dividing by Size would under-report.
 *
 * statvfs is a libc call (the same one `df` uses), so it needs `unsafe`:
 * Rust can't check what the C side does with the pointers we hand it. */
fn disk_used_percent(mount_point: &str) -> Option<f64> {
    let path = std::ffi::CString::new(mount_point).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string that outlives the
    // call, and `stat` is a properly sized struct statvfs that we own;
    // statvfs only writes into it.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let used = stat.f_blocks.checked_sub(stat.f_bfree)? as f64;
    let avail = stat.f_bavail as f64;
    if used + avail == 0.0 {
        return None;
    }
    Some(round1(100.0 * used / (used + avail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Real contents captured from the DK2 (2026-09-23). */
    const STAT: &str = "cpu  1554 4 2134 42699 147 0 14 0 0 0\n\
                        cpu0 721 4 1002 21468 50 0 12 0 0 0\n\
                        cpu1 833 0 1132 21230 96 0 2 0 0 0\n\
                        intr 12345 0 0\n";
    const MEMINFO: &str = "MemTotal:         395832 kB\n\
                           MemFree:          250000 kB\n\
                           MemAvailable:     325500 kB\n\
                           Buffers:            1234 kB\n";

    #[test]
    fn cpu_times_use_the_summary_line_and_first_eight_columns() {
        let t = parse_cpu_times(STAT).unwrap();
        assert_eq!(t.idle, 42699 + 147);
        /* user+nice+system+idle+iowait+irq+softirq+steal
         * = 1554+4+2134+42699+147+0+14+0; guest columns excluded */
        assert_eq!(t.total, 46552);
    }

    #[test]
    fn cpu_percent_is_busy_share_of_the_interval() {
        /* 1000 ticks passed, 750 of them idle -> 25% busy. */
        let prev = CpuTimes { idle: 5000, total: 10000 };
        let now = CpuTimes { idle: 5750, total: 11000 };
        assert_eq!(cpu_percent(prev, now), Some(25.0));
    }

    #[test]
    fn cpu_percent_is_none_when_no_time_passed_or_counters_go_back() {
        let t = CpuTimes { idle: 5, total: 10 };
        assert_eq!(cpu_percent(t, t), None);
        let earlier = CpuTimes { idle: 1, total: 2 };
        assert_eq!(cpu_percent(t, earlier), None);
    }

    #[test]
    fn meminfo_reads_total_and_available() {
        let m = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(m.total_kb, 395832);
        assert_eq!(m.available_kb, 325500);
    }

    #[test]
    fn meminfo_without_memavailable_is_none() {
        assert!(parse_meminfo("MemTotal: 100 kB\nMemFree: 50 kB\n").is_none());
    }

    #[test]
    fn temperature_and_uptime_parse() {
        assert_eq!(parse_millidegrees("45538\n"), Some(45.5));
        assert_eq!(parse_uptime("234.14 427.00\n"), Some(234));
        assert_eq!(parse_millidegrees("garbage"), None);
    }

    #[test]
    fn sample_never_fails_and_has_every_field() {
        /* On the dev PC some paths differ (no remoteproc, maybe no thermal
         * zone) -- those must come back as null, not break the object. */
        let mut s = Sampler::new(Arc::new(AtomicUsize::new(2)));
        let first = s.sample(3);
        assert!(first["cpu_percent"].is_null()); /* nothing to compare yet */
        assert_eq!(first["device_count"], 3);
        assert_eq!(first["local_clients"], 2);
        for key in ["memory", "temperature_c", "disk", "uptime_s", "m4", "version"] {
            assert!(first.get(key).is_some(), "missing {key}");
        }
    }
}
