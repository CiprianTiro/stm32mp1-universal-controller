/*
 * power_link.rs -- "powered by" a plug (issue #99): a device that can't
 * say what it's doing (an IR LED strip, an IR projector or fan: IR goes
 * one way) paired with the PLUG that powers it. The hub then has more
 * control over it, and -- with a plug that measures power -- KNOWS
 * whether it's on.
 *
 * THE LINK, in the IR device's config (set_link; ws "set_power_link"):
 *   powered_by      the plug's device id
 *   cut_power       "on": switching the device off also cuts the plug
 *                   (no standby draw); "off": the plug stays on
 *   start_delay_s   how long the device needs once it has power before
 *                   it listens to IR (a strip: ~1 s; a projector more)
 *   power_off_w / power_on_w / power_threshold_w
 *                   what the plug measured with the device off and on,
 *                   and the line between them (learn(); ws "learn_power")
 *   power_problem   why the last command didn't work, for the screens
 *                   (cleared by the next one that does)
 *
 * MASTER POWER (any plug, EZVIZ included). Every `switch` command for a
 * linked device comes here (control.rs), whoever sends it -- screen, app,
 * cloud, scene, automation:
 *   on    the plug off? switch it on, wait start_delay_s; then the IR
 *         "on" (an IR light: its adapter presses; any other IR device:
 *         the remote's On / Power button, pressed from here)
 *   off   the IR "off"; then, with cut_power, the plug off
 * A plug that's off means the device is off, whatever the hub assumed.
 *
 * CONFIRMED STATE (only with a plug that measures: Shelly, EZVIZ
 * T30-10B, Tasmota with metering). Once learned, the device's on/off
 * FOLLOWS THE MEASURED POWER (run(): every reading of the plug) --
 * including when someone uses the original remote. And it makes a single
 * Power TOGGLE button safe: the IR task presses it only if the measured
 * state says it's needed (ir_light::presses_for_switch). After each
 * command the hub checks the power really rose or fell (verify()); if
 * not, it sends once more; if still nothing, the device shows what's
 * measured and power_problem says so.
 *
 * WHICH DEVICES: IR blaster devices only (the ones without a state of
 * their own); the plug: any device with a `switch` that isn't linked
 * itself. Readings: a plug that can be asked "read now" (Registry::
 * refresh: Shelly via the generic HTTP adapter, EZVIZ) is asked after
 * each command; any other is waited for (its own polling).
 */
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
/* tokio's clock (the same as std's in the hub): the tests run it paused. */
use tokio::time::Instant;

use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::control::Control;
use crate::device::Device;
use crate::ir_light::LightMap;
use crate::state::Event;

pub const POWERED_BY: &str = "powered_by";
pub const CUT_POWER: &str = "cut_power";
pub const START_DELAY: &str = "start_delay_s";
pub const OFF_W: &str = "power_off_w";
pub const ON_W: &str = "power_on_w";
pub const THRESHOLD: &str = "power_threshold_w";
pub const PROBLEM: &str = "power_problem";
/* All of them: what unlinking removes. */
const KEYS: [&str; 7] = [POWERED_BY, CUT_POWER, START_DELAY, OFF_W, ON_W, THRESHOLD, PROBLEM];
/* What a new plug makes meaningless (it measures differently). */
const LEARNED: [&str; 4] = [OFF_W, ON_W, THRESHOLD, PROBLEM];

/* The only adapter whose devices can be linked (see the header). */
const IR_ADAPTER: &str = "ir-blaster";

const DEFAULT_START_DELAY_S: u64 = 2;
const MAX_START_DELAY_S: u64 = 60;
/* Between the IR "off" and cutting the plug: let the code go out. */
const BEFORE_CUT: Duration = Duration::from_millis(500);
/* After a command, before reading the plug: the device changes its draw
 * (at least this, or start_delay_s if longer: a slow starter). */
const DEVICE_SETTLE: Duration = Duration::from_secs(2);
/* HOW OLD A PLUG'S READING IS: after a change, how long until its
 * reading shows it. A local plug asked to "read now" (a Shelly): a few
 * seconds. A CLOUD plug: the plug uploads its power to the vendor only
 * every so often, and "read now" just reads the cloud's copy -- EZVIZ's
 * lags ~20-25 s (measured 2026-10-07; deciding on a reading taken
 * earlier sent an IR toggle again that had worked: it switched the
 * strip back). A plug that can't be asked: its own polling. */
const LOCAL_READING_LAG: Duration = Duration::from_secs(3);
const CLOUD_READING_LAG: Duration = Duration::from_secs(30);
const NO_REFRESH_LAG: Duration = Duration::from_secs(35);
/* The adapters whose plugs' readings come through a cloud. */
const CLOUD_PLUGS: [&str; 1] = ["ezviz"];
/* Above this lag, power_up() doesn't wait for a reading before the IR
 * "on" (it would hold the command half a minute): verify() catches a
 * device that came back on by itself instead. */
const POWER_UP_MAX_LAG: Duration = Duration::from_secs(5);
/* While checking a command: how often the hub's copy of the reading is
 * looked at, and a cloud plug asked again. */
const CHECK_EVERY: Duration = Duration::from_secs(1);
const ASK_AGAIN: Duration = Duration::from_secs(10);
/* Off and on must differ by at least this much (W) and this factor to
 * be told apart (a reading jitters by a few tenths of a watt). */
const MIN_GAP_W: f64 = 1.0;
const MIN_RATIO: f64 = 1.5;

/* A device's link, read from its config. */
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub plug: String,
    pub cut_power: bool,
    pub start_delay: Duration,
    /* Learned: the line between off and on. None = not learned (state
     * assumed). */
    pub threshold: Option<f64>,
    /* What off and on measured, for the margin around the line. */
    pub off_w: Option<f64>,
    pub on_w: Option<f64>,
}

pub fn link_of(device: &Device) -> Option<Link> {
    let config = &device.config;
    let plug = config.get(POWERED_BY).filter(|p| !p.is_empty())?.clone();
    let start_delay_s = config.get(START_DELAY).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_START_DELAY_S);
    Some(Link {
        plug,
        cut_power: config.get(CUT_POWER).map(String::as_str) == Some("on"),
        start_delay: Duration::from_secs(start_delay_s.min(MAX_START_DELAY_S)),
        threshold: config.get(THRESHOLD).and_then(|t| t.parse().ok()),
        off_w: config.get(OFF_W).and_then(|t| t.parse().ok()),
        on_w: config.get(ON_W).and_then(|t| t.parse().ok()),
    })
}

/* On or off, as the plug's measurement says (None: can't tell -- not
 * learned, or the plug doesn't measure). A plug that's OFF always means
 * off: no power, no device. */
pub fn measured_on(link: &Link, plug: &Device) -> Option<bool> {
    if !plug_is_on(plug) {
        return Some(false);
    }
    watts_say(link, plug.capabilities.energy.as_ref()?.power_w)
}

/* On or off for `watts`, with a MARGIN around the learned line: a
 * quarter of the gap between off and on each side. A reading inside it
 * (a device starting up, a jittering one) says nothing, so the state
 * doesn't flicker. */
pub fn watts_say(link: &Link, watts: f64) -> Option<bool> {
    let threshold = link.threshold?;
    let margin = match (link.off_w, link.on_w) {
        (Some(off), Some(on)) if on > off => (on - off) / 4.0,
        _ => 0.0,
    };
    if watts >= threshold + margin {
        Some(true)
    } else if watts <= threshold - margin {
        Some(false)
    } else {
        None
    }
}

/* The line between off and on, if the two readings can be told apart. */
pub fn threshold_between(off_w: f64, on_w: f64) -> Option<f64> {
    (on_w - off_w >= MIN_GAP_W && on_w >= off_w * MIN_RATIO).then(|| ((off_w + on_w) / 2.0 * 10.0).round() / 10.0)
}

fn plug_is_on(plug: &Device) -> bool {
    plug.capabilities.switch.as_ref().is_some_and(|s| s.on)
}

fn device_is_on(device: &Device) -> bool {
    device.capabilities.switch.as_ref().is_some_and(|s| s.on)
}

/* Does the device's own adapter carry out `switch` (an IR light, issue
 * #85), or are the remote's buttons pressed from here? */
fn adapter_switches(device: &Device) -> bool {
    device.config.get("light").map(String::as_str) == Some("on")
}

/* ------------------------------------------------------------------ */
/* Devices with a command in progress                                  */
/* ------------------------------------------------------------------ */

/* While a command or a learning runs, the device's state is this
 * module's business: run() doesn't follow the readings (the strip hasn't
 * started yet; a cloud plug's reading is still the old one).
 *
 * ONLY THE NEWEST COMMAND COUNTS: each gets a number; a newer command for
 * the same device makes an older one's check stop (is_current), so taps
 * in quick succession don't each send the code again (seen on the DK2:
 * ten checks at once, each pressing the Power toggle). Per device: its
 * newest command's number, and until when it may keep the device busy. */
static BUSY: Mutex<Option<HashMap<String, (u64, Instant)>>> = Mutex::new(None);
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/* Longest a command may keep a device busy (a lost task never blocks
 * following the readings for long). */
const BUSY_MAX: Duration = Duration::from_secs(180);

fn is_busy(id: &str) -> bool {
    BUSY.lock().unwrap().as_ref().and_then(|b| b.get(id)).is_some_and(|(_, until)| Instant::now() < *until)
}

/* The device's busy mark, while it lives (dropped: cleared -- unless a
 * newer command took over). */
struct Busy {
    id: String,
    number: u64,
}

impl Busy {
    fn new(id: &str) -> Busy {
        let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        BUSY.lock().unwrap().get_or_insert_with(HashMap::new).insert(id.to_string(), (number, Instant::now() + BUSY_MAX));
        Busy { id: id.to_string(), number }
    }

    /* Still the device's newest command? */
    fn is_current(&self) -> bool {
        BUSY.lock().unwrap().as_ref().and_then(|b| b.get(&self.id)).is_some_and(|(n, _)| *n == self.number)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(busy) = BUSY.lock().unwrap().as_mut() {
            if busy.get(&self.id).is_some_and(|(n, _)| *n == self.number) {
                busy.remove(&self.id);
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* Commands                                                            */
/* ------------------------------------------------------------------ */

/* A `switch` command for a linked device (control.rs, after the rules
 * were checked). See the header: MASTER POWER. */
pub async fn switch(control: &Control, device: &Device, link: &Link, on: bool) -> Result<(), String> {
    let plug = control.get(&link.plug).await?.ok_or_else(|| format!("its plug {:?} is gone", link.plug))?;
    let busy = Busy::new(&device.id);
    let cut = !on && link.cut_power;
    if on {
        power_up(control, device, link, &plug).await?;
    }
    /* With the plug cut right after, an IR "off" isn't needed by the
     * device -- but sent anyway: a TV then shuts down properly first. */
    let pressed = drive(control, &device.id, on).await;
    match (&pressed, cut) {
        (Err(why), false) => return Err(why.clone()),
        /* Cut anyway: that switches it off for sure. */
        (Err(why), true) => println!("power_link: {}: {why} (cutting the power anyway)", device.id),
        (Ok(_), _) => {}
    }
    if cut && plug_is_on(&plug) {
        tokio::time::sleep(BEFORE_CUT).await;
        control.command(&link.plug, "switch", json!({ "on": false })).await?;
    }
    if cut {
        control.report(&device.id, "switch", json!({ "on": false })).await?;
    }
    let _ = control.remove_config(&device.id, &[PROBLEM]).await;
    /* Measured afterwards (not when the plug was just cut: no power is
     * as confirmed as it gets). */
    if link.threshold.is_some() && !cut {
        let (control, id, link) = (control.clone(), device.id.clone(), link.clone());
        tokio::spawn(async move {
            verify(&control, &id, &link, on, &busy).await;
        });
    }
    Ok(())
}

/* Before switching on: the plug on, if it isn't, and the device given
 * its start delay. It was without power, so it was OFF -- or, for one
 * that comes back on by itself (many strips do), what the plug then
 * measures: so a Power toggle is pressed only if needed. */
async fn power_up(control: &Control, device: &Device, link: &Link, plug: &Device) -> Result<(), String> {
    if plug_is_on(plug) {
        return Ok(());
    }
    control.command(&link.plug, "switch", json!({ "on": true })).await?;
    control.report(&device.id, "switch", json!({ "on": false })).await?;
    tokio::time::sleep(link.start_delay).await;
    if link.threshold.is_some() && reading_lag(control, plug) <= POWER_UP_MAX_LAG {
        if let Some(watts) = read_power(control, plug).await {
            if watts_say(link, watts) == Some(true) {
                control.report(&device.id, "switch", json!({ "on": true })).await?;
            }
        }
    }
    Ok(())
}

/* The IR side of on / off: an IR light's adapter does it (and reports);
 * for any other IR device the remote's buttons are pressed from here
 * (On / Off, or Power if the hub's state says it's needed), and the
 * state reported. Ok(true): something was sent. */
async fn drive(control: &Control, id: &str, on: bool) -> Result<bool, String> {
    let device = control.get(id).await?.ok_or_else(|| format!("{id} is gone"))?;
    if adapter_switches(&device) {
        control.adapters().command(id, "switch", json!({ "on": on })).await?;
        return Ok(true);
    }
    let map = LightMap::from_buttons(&control.ir_codes().get(id));
    /* No button that could switch it off: only cutting the plug can
     * (switching on is the plug's power coming back, buttons or not). */
    if !on && map.off.is_none() && map.toggle.is_none() {
        return Err("it has no Off or Power button: turn on \"Cut the power when off\", or teach one".into());
    }
    let presses = map.presses_for_switch(on, device_is_on(&device));
    for button in &presses {
        control.adapters().action(id, "remote", "press", json!({ "button": button })).await?;
    }
    control.report(id, "switch", json!({ "on": on })).await?;
    Ok(!presses.is_empty())
}

/* After a command: did the power really change? Watched until the plug's
 * reading can show it (reading_lag), accepted as soon as it does. If it
 * doesn't, once more (with the hub's state set to what's measured, so a
 * toggle is pressed again); then the measured state, and why. Stops as
 * soon as a newer command for the device came (`busy`). */
async fn verify(control: &Control, id: &str, link: &Link, on: bool, busy: &Busy) {
    for attempt in 1..=2 {
        tokio::time::sleep(DEVICE_SETTLE.max(link.start_delay)).await;
        let Some(watts) = watch_power(control, link, on, busy).await else { return };
        if watts_say(link, watts) != Some(!on) {
            /* As asked -- or inside the margin: not clearly wrong. */
            return;
        }
        if !busy.is_current() {
            return;
        }
        let _ = control.report(id, "switch", json!({ "on": !on })).await;
        if attempt == 1 {
            println!("power_link: {id}: still {watts} W after switching it {}: sending again", if on { "on" } else { "off" });
            if let Err(why) = drive(control, id, on).await {
                println!("power_link: {id}: {why}");
                return;
            }
        } else {
            let why = format!("didn't switch {} (the plug measures {watts} W)", if on { "on" } else { "off" });
            println!("power_link: {id}: {why}");
            let _ = control.store_config(id, [(PROBLEM.to_string(), why)].into()).await;
        }
    }
}

/* How long until `plug`'s reading shows a change (see the constants). */
fn reading_lag(control: &Control, plug: &Device) -> Duration {
    if CLOUD_PLUGS.contains(&plug.source.as_str()) {
        CLOUD_READING_LAG
    } else if control.adapters().can_refresh(&plug.id) {
        LOCAL_READING_LAG
    } else {
        NO_REFRESH_LAG
    }
}

/* The plug's power once its reading can show what just happened. A
 * plug that can be asked is asked at the END of its lag (a cloud plug
 * asked at once would hand over its old copy). None: it doesn't measure. */
async fn read_power(control: &Control, plug: &Device) -> Option<f64> {
    let lag = reading_lag(control, plug);
    if control.adapters().can_refresh(&plug.id) {
        tokio::time::sleep(lag.saturating_sub(LOCAL_READING_LAG)).await;
        control.refresh(&plug.id);
        tokio::time::sleep(LOCAL_READING_LAG).await;
    } else {
        tokio::time::sleep(lag).await;
    }
    power_now(control, &plug.id).await
}

/* The hub's copy of the plug's reading, as it is now. */
async fn power_now(control: &Control, plug: &str) -> Option<f64> {
    control.get(plug).await.ok()??.capabilities.energy.map(|e| e.power_w)
}

/* Watches the plug's reading for up to its lag: returns as soon as it
 * says `on` (as asked), else the last reading. None: no reading, or a
 * newer command took over. */
async fn watch_power(control: &Control, link: &Link, on: bool, busy: &Busy) -> Option<f64> {
    let plug = control.get(&link.plug).await.ok()??;
    let deadline = Instant::now() + reading_lag(control, &plug);
    let mut asked = Instant::now();
    control.refresh(&link.plug);
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if !busy.is_current() {
            return None;
        }
        let watts = power_now(control, &link.plug).await?;
        if watts_say(link, watts) == Some(on) || Instant::now() >= deadline {
            return Some(watts);
        }
        if asked.elapsed() >= ASK_AGAIN {
            control.refresh(&link.plug);
            asked = Instant::now();
        }
    }
}

/* ------------------------------------------------------------------ */
/* Setting the link, learning                                          */
/* ------------------------------------------------------------------ */

/* Links `id` to `plug` (None: unlinks). A linked IR device always has a
 * `switch`: master power, even with no button for it. */
pub async fn set_link(control: &Control, id: &str, plug: Option<&str>, cut_power: bool, start_delay_s: Option<u64>) -> Result<Device, String> {
    let device = control.get(id).await?.ok_or_else(|| format!("unknown device {id:?}"))?;
    let Some(plug) = plug.filter(|p| !p.is_empty()) else {
        let device = control.remove_config(id, &KEYS).await?;
        if !adapter_switches(&device) {
            control.remove_capabilities(id, vec!["switch".into()]).await?;
        }
        return Ok(control.get(id).await?.unwrap_or(device));
    };
    if device.source.as_str() != IR_ADAPTER {
        return Err("only IR devices can be powered by a plug (other devices know their own state)".into());
    }
    if plug == id {
        return Err("a device can't be its own plug".into());
    }
    let plug_device = control.get(plug).await?.ok_or_else(|| format!("unknown plug {plug:?}"))?;
    if plug_device.capabilities.switch.is_none() {
        return Err(format!("{} can't be switched on and off", plug_device.name));
    }
    if link_of(&plug_device).is_some() || plug_device.source.as_str() == IR_ADAPTER {
        return Err(format!("{} isn't a plug", plug_device.name));
    }
    if link_of(&device).is_some_and(|l| l.plug != plug) {
        control.remove_config(id, &LEARNED).await?;
    }
    let delay = start_delay_s.unwrap_or(DEFAULT_START_DELAY_S).min(MAX_START_DELAY_S);
    let config = [
        (POWERED_BY.to_string(), plug.to_string()),
        (CUT_POWER.to_string(), if cut_power { "on" } else { "off" }.to_string()),
        (START_DELAY.to_string(), delay.to_string()),
    ];
    control.store_config(id, config.into()).await?;
    control.add_missing_capabilities(id, vec!["switch".into()]).await?;
    control.get(id).await?.ok_or_else(|| format!("{id} disappeared"))
}

/* Guided learning, one step at a time (the person watches the device):
 *   "off"     the PERSON has made sure it's off (with its remote if
 *             needed); the hub records that and measures -> power_off_w.
 *             Not switched off by the hub: with only a Power toggle, the
 *             hub can't know whether a press would switch it off or on
 *             (a strip may have come back on by itself)
 *   "on"      the hub switches it on -- from the now known "off", so a
 *             toggle is pressed right -- and measures -> power_on_w, and
 *             the threshold if the two can be told apart
 *   "forget"  back to an assumed state
 * Answers {"watts": ..} (and "threshold" after "on"). */
pub async fn learn(control: &Control, id: &str, step: &str) -> Result<Value, String> {
    let device = control.get(id).await?.ok_or_else(|| format!("unknown device {id:?}"))?;
    let link = link_of(&device).ok_or("it isn't powered by a plug yet")?;
    if step == "forget" {
        control.remove_config(id, &LEARNED).await?;
        return Ok(json!({}));
    }
    let on = match step {
        "off" => false,
        "on" => true,
        other => return Err(format!("no learning step {other:?} (off, on, forget)")),
    };
    let plug = control.get(&link.plug).await?.ok_or_else(|| format!("its plug {:?} is gone", link.plug))?;
    if plug.capabilities.energy.is_none() {
        return Err(format!("{} doesn't measure power: the state stays assumed", plug.name));
    }
    let _busy = Busy::new(id);
    /* Not learned while learning: the plug's last reading mustn't decide. */
    let unlearned = Link { threshold: None, ..link.clone() };
    power_up(control, &device, &unlearned, &plug).await?;
    if on {
        drive(control, id, true).await?;
    } else {
        control.report(id, "switch", json!({ "on": false })).await?;
    }
    tokio::time::sleep(DEVICE_SETTLE.max(link.start_delay)).await;
    let watts = read_power(control, &plug).await.ok_or("the plug gave no reading")?;
    let key = if on { ON_W } else { OFF_W };
    control.store_config(id, [(key.to_string(), watts.to_string())].into()).await?;
    if !on {
        return Ok(json!({ "watts": watts }));
    }
    let off_w: f64 = device.config.get(OFF_W).and_then(|w| w.parse().ok()).ok_or("measure it off first")?;
    match threshold_between(off_w, watts) {
        Some(threshold) => {
            control.store_config(id, [(THRESHOLD.to_string(), threshold.to_string())].into()).await?;
            let _ = control.remove_config(id, &[PROBLEM]).await;
            Ok(json!({ "watts": watts, "threshold": threshold }))
        }
        None => {
            control.remove_config(id, &[THRESHOLD]).await?;
            Err(format!(
                "off {off_w} W and on {watts} W are too close to tell apart -- was it really on? (the state stays assumed)"
            ))
        }
    }
}

/* ------------------------------------------------------------------ */
/* Following the plugs                                                 */
/* ------------------------------------------------------------------ */

/* Runs for the daemon's life: every change of a plug -> the devices it
 * powers follow its measurement (see the header); a removed plug ->
 * their links are undone. */
pub async fn run(control: Control, mut events: broadcast::Receiver<Event>) {
    loop {
        match events.recv().await {
            Ok(Event::Changed(device)) => {
                if device.capabilities.switch.is_some() {
                    follow(&control, &device).await;
                }
            }
            Ok(Event::Removed(id)) => unlink_all(&control, &id).await,
            /* Missed some: the next reading catches up. */
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/* The devices `plug` powers, with their links. */
async fn powered_by(control: &Control, plug: &str) -> Vec<(Device, Link)> {
    let Ok(devices) = control.list().await else { return Vec::new() };
    devices
        .into_iter()
        .filter_map(|d| link_of(&d).filter(|l| l.plug == plug).map(|l| (d, l)))
        .collect()
}

async fn follow(control: &Control, plug: &Device) {
    for (device, link) in powered_by(control, &plug.id).await {
        if is_busy(&device.id) {
            continue;
        }
        if let Some(on) = measured_on(&link, plug) {
            if on != device_is_on(&device) {
                println!("power_link: {} is {} (measured by {})", device.id, if on { "on" } else { "off" }, plug.id);
                if let Err(e) = control.report(&device.id, "switch", json!({ "on": on })).await {
                    println!("power_link: {}: {e}", device.id);
                }
            }
        }
    }
}

async fn unlink_all(control: &Control, plug: &str) {
    for (device, _) in powered_by(control, plug).await {
        println!("power_link: {}: its plug {plug} was removed: unlinked", device.id);
        if let Err(e) = set_link(control, &device.id, None, false, None).await {
            println!("power_link: {}: {e}", device.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(config: &[(&str, &str)]) -> Device {
        let mut d: Device = serde_json::from_value(json!({
            "id": "strip", "name": "Strip", "source": "ir-blaster",
            "capabilities": {"switch": {"on": false}, "remote": {"buttons": []}}
        }))
        .unwrap();
        d.config = config.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        d
    }

    fn plug(on: bool, watts: Option<f64>) -> Device {
        let mut caps = json!({"switch": {"on": on}});
        if let Some(w) = watts {
            caps["energy"] = json!({"power_w": w});
        }
        serde_json::from_value(json!({"id": "plug", "name": "Plug", "capabilities": caps})).unwrap()
    }

    #[test]
    fn link_from_config() {
        assert_eq!(link_of(&device(&[])), None);
        let link = link_of(&device(&[(POWERED_BY, "plug"), (CUT_POWER, "on"), (START_DELAY, "5"), (THRESHOLD, "3.2")])).unwrap();
        assert_eq!(
            link,
            Link { plug: "plug".into(), cut_power: true, start_delay: Duration::from_secs(5), threshold: Some(3.2), off_w: None, on_w: None }
        );
        /* Defaults, and a start delay kept within bounds. */
        let link = link_of(&device(&[(POWERED_BY, "plug"), (START_DELAY, "9999")])).unwrap();
        assert!(!link.cut_power && link.threshold.is_none());
        assert_eq!(link.start_delay, Duration::from_secs(MAX_START_DELAY_S));
        assert_eq!(link_of(&device(&[(POWERED_BY, "plug")])).unwrap().start_delay, Duration::from_secs(DEFAULT_START_DELAY_S));
    }

    #[test]
    fn measured_state() {
        let learned = link_of(&device(&[(POWERED_BY, "plug"), (THRESHOLD, "3.2")])).unwrap();
        let assumed = link_of(&device(&[(POWERED_BY, "plug")])).unwrap();
        /* A plug that's off: off, learned or not. */
        assert_eq!(measured_on(&learned, &plug(false, Some(0.0))), Some(false));
        assert_eq!(measured_on(&assumed, &plug(false, None)), Some(false));
        /* On: the measurement decides, if there is one and it's learned. */
        assert_eq!(measured_on(&learned, &plug(true, Some(6.1))), Some(true));
        assert_eq!(measured_on(&learned, &plug(true, Some(0.4))), Some(false));
        assert_eq!(measured_on(&assumed, &plug(true, Some(6.1))), None);
        assert_eq!(measured_on(&learned, &plug(true, None)), None);
    }

    #[test]
    fn a_margin_around_the_line() {
        /* 0.4 W off, 6 W on: line 3.2 W, margin 1.4 W each side. */
        let link = link_of(&device(&[(POWERED_BY, "plug"), (OFF_W, "0.4"), (ON_W, "6"), (THRESHOLD, "3.2")])).unwrap();
        assert_eq!(watts_say(&link, 6.0), Some(true));
        assert_eq!(watts_say(&link, 4.7), Some(true));
        assert_eq!(watts_say(&link, 4.0), None);
        assert_eq!(watts_say(&link, 2.5), None);
        assert_eq!(watts_say(&link, 1.7), Some(false));
        assert_eq!(watts_say(&link, 0.4), Some(false));
        /* Not learned: nothing to say. */
        assert_eq!(watts_say(&link_of(&device(&[(POWERED_BY, "plug")])).unwrap(), 6.0), None);
    }

    #[test]
    fn thresholds() {
        /* The test strip: 0.4 W standby, 6 W on. */
        assert_eq!(threshold_between(0.4, 6.0), Some(3.2));
        /* Too close, or less on than off. */
        assert_eq!(threshold_between(0.4, 1.0), None);
        assert_eq!(threshold_between(60.0, 70.0), None);
        assert_eq!(threshold_between(6.0, 0.4), None);
        /* A TV: 0.5 W standby, 80 W on. */
        assert_eq!(threshold_between(0.5, 80.0), Some(40.3));
    }

    #[test]
    fn busy_marks_end() {
        {
            let _busy = Busy::new("busy-test");
            assert!(is_busy("busy-test"));
        }
        assert!(!is_busy("busy-test"));
    }

    /* ---------------------------------------------------------------- */
    /* The whole path: a simulated LED strip on a simulated plug         */
    /* ---------------------------------------------------------------- */

    use crate::adapters::{Adapter, DeviceCmd, DeviceHandle, Hub, Registry};
    use crate::device::{Capabilities, Source};
    use crate::secrets::Secrets;
    use crate::state::{self, Outputs};
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch, Notify};

    const STANDBY_W: f64 = 0.4;
    const STRIP_ON_W: f64 = 6.0;

    /* The real world both fakes act on. */
    #[derive(Default)]
    struct World {
        plug_on: bool,
        strip_on: bool,
        /* Back on by itself when the power returns (many strips are). */
        comes_back_on: bool,
        /* Misses the next N IR codes (someone in the way). */
        deaf: u32,
        presses: Vec<String>,
        /* A cloud plug: its reading shows a change only this much later
         * (EZVIZ: ~20-25 s); until then, the one before. */
        lag: Duration,
        changed: Option<(Instant, f64)>,
    }

    impl World {
        fn watts(&self) -> f64 {
            match (self.plug_on, self.strip_on) {
                (false, _) => 0.0,
                (true, false) => STANDBY_W,
                (true, true) => STRIP_ON_W,
            }
        }

        /* What the plug reports now (lagging, see `lag`). */
        fn reading(&self) -> f64 {
            match self.changed {
                Some((at, before)) if at.elapsed() < self.lag => before,
                _ => self.watts(),
            }
        }

        /* Call BEFORE changing the world: remembers the reading so far. */
        fn changing(&mut self) {
            self.changed = Some((Instant::now(), self.reading()));
        }
    }

    type Shared = Arc<Mutex<World>>;

    /* The IR side: presses the remote's buttons (no light mode). */
    struct FakeIr(Shared);

    impl Adapter for FakeIr {
        fn id(&self) -> &'static str {
            IR_ADAPTER
        }

        fn start(&self, _device: &Device, _hub: Hub) -> DeviceHandle {
            let (tx, mut rx) = mpsc::channel(8);
            let world = self.0.clone();
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        DeviceCmd::Action { name, args, reply, .. } if name == "press" => {
                            let button = args["button"].as_str().unwrap().to_string();
                            let mut w = world.lock().unwrap();
                            w.presses.push(button.clone());
                            w.changing();
                            if w.deaf > 0 {
                                w.deaf -= 1;
                            } else if w.plug_on {
                                w.strip_on = match button.as_str() {
                                    "Power" => !w.strip_on,
                                    "On" => true,
                                    "Off" => false,
                                    _ => w.strip_on,
                                };
                            }
                            let _ = reply.send(Ok(json!({})));
                        }
                        other => other.refuse("not in this test"),
                    }
                }
            });
            DeviceHandle::new(tx)
        }
    }

    /* The plug: switches, and reports its power when asked ("read now").
     * Its adapter id: "fake-plug", or "ezviz" for a cloud plug. */
    struct FakePlug(Shared, &'static str);

    impl Adapter for FakePlug {
        fn id(&self) -> &'static str {
            self.1
        }

        fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
            let (tx, mut rx) = mpsc::channel(8);
            let refresh = Arc::new(Notify::new());
            let (world, id, notified) = (self.0.clone(), device.id.clone(), refresh.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        cmd = rx.recv() => {
                            let Some(cmd) = cmd else { return };
                            let DeviceCmd::Command { value, reply, .. } = cmd else { cmd.refuse("no"); continue };
                            let on = value["on"].as_bool().unwrap();
                            {
                                let mut w = world.lock().unwrap();
                                w.changing();
                                w.plug_on = on;
                                w.strip_on = on && w.comes_back_on;
                            }
                            hub.report(&id, "switch", json!({ "on": on })).await.unwrap();
                            let _ = reply.send(Ok(()));
                        }
                        _ = notified.notified() => {}
                    }
                    let watts = world.lock().unwrap().reading();
                    hub.report(&id, "energy", json!({ "power_w": watts })).await.unwrap();
                }
            });
            DeviceHandle::new(tx).with_refresh(refresh)
        }
    }

    fn new_device(id: &str, adapter: &str, capabilities: &[&str]) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            room: String::new(),
            template: String::new(),
            source: Source::new(adapter),
            config: Default::default(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: Capabilities::with_defaults(&capabilities.iter().map(|c| c.to_string()).collect::<Vec<_>>()).unwrap(),
        }
    }

    /* A hub with `strip` (taught `buttons`) and `plug`, power_link
     * following the plug, and the world they share. */
    async fn hub(strip: &str, buttons: &[&str], world: World) -> (Control, Shared) {
        /* A plug whose reading lags is a cloud plug. */
        let plug_adapter = if world.lag.is_zero() { "fake-plug" } else { "ezviz" };
        let world = Arc::new(Mutex::new(world));
        let devices = [new_device(strip, IR_ADAPTER, &["remote"]), new_device("plug", plug_adapter, &["switch", "energy"])];
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, _) = broadcast::channel(64);
        let outputs = Outputs { changed_tx: watch::channel(()).0, events_tx: events_tx.clone(), save_tx: watch::channel(Vec::new()).0 };
        tokio::spawn(state::run(state_rx, devices.into_iter().map(|d| (d.id.clone(), d)).collect(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(FakeIr(world.clone())), Box::new(FakePlug(world.clone(), plug_adapter))]));
        let control = Control::new(state_tx, registry.clone(), Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0)));
        for button in buttons {
            control.ir_codes().learn(strip, button, json!({}));
        }
        registry.start_all(&control).await;
        tokio::spawn(run(control.clone(), events_tx.subscribe()));
        /* The plug's first state (its task reports on its own command). */
        let on = world.lock().unwrap().plug_on;
        control.adapters().command("plug", "switch", json!({ "on": on })).await.unwrap();
        (control, world)
    }

    async fn is_on(control: &Control, id: &str) -> bool {
        device_is_on(&control.get(id).await.unwrap().unwrap())
    }

    async fn config(control: &Control, id: &str, key: &str) -> Option<String> {
        control.get(id).await.unwrap().unwrap().config.get(key).cloned()
    }

    /* Long enough for any verification to finish (simulated time). */
    async fn settle() {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn master_power_on_then_cut() {
        let (control, world) = hub("strip-a", &["Power"], World::default()).await;
        set_link(&control, "strip-a", Some("plug"), true, Some(1)).await.unwrap();
        /* Power on: the plug first, then the Power toggle (it was off). */
        control.command("strip-a", "switch", json!({ "on": true })).await.unwrap();
        assert!(world.lock().unwrap().strip_on);
        assert!(is_on(&control, "strip-a").await);
        /* Off: the toggle, then the plug cut. */
        control.command("strip-a", "switch", json!({ "on": false })).await.unwrap();
        let w = world.lock().unwrap();
        assert!(!w.plug_on && !w.strip_on);
        assert_eq!(w.presses, vec!["Power", "Power"]);
        drop(w);
        assert!(!is_on(&control, "strip-a").await);
    }

    #[tokio::test(start_paused = true)]
    async fn no_off_button_needs_the_cut() {
        let (control, _) = hub("strip-b", &["On"], World { plug_on: true, ..Default::default() }).await;
        set_link(&control, "strip-b", Some("plug"), false, None).await.unwrap();
        let err = control.command("strip-b", "switch", json!({ "on": false })).await.unwrap_err();
        assert!(err.contains("Cut the power"), "{err}");
        /* With the cut it works. */
        set_link(&control, "strip-b", Some("plug"), true, None).await.unwrap();
        control.command("strip-b", "switch", json!({ "on": false })).await.unwrap();
        assert!(!is_on(&control, "strip-b").await);
    }

    #[tokio::test(start_paused = true)]
    async fn learned_state_follows_the_real_remote() {
        let (control, world) = hub("strip-c", &["Power"], World { plug_on: true, ..Default::default() }).await;
        set_link(&control, "strip-c", Some("plug"), false, Some(1)).await.unwrap();
        assert_eq!(learn(&control, "strip-c", "off").await.unwrap(), json!({ "watts": STANDBY_W }));
        assert_eq!(learn(&control, "strip-c", "on").await.unwrap(), json!({ "watts": STRIP_ON_W, "threshold": 3.2 }));
        assert_eq!(config(&control, "strip-c", THRESHOLD).await.as_deref(), Some("3.2"));
        /* Someone switches it off with the original remote: the hub
         * follows the next reading. */
        world.lock().unwrap().strip_on = false;
        control.refresh("plug");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!is_on(&control, "strip-c").await);
        /* ...so the Power toggle is pressed again to switch it on. */
        control.command("strip-c", "switch", json!({ "on": true })).await.unwrap();
        settle().await;
        assert!(world.lock().unwrap().strip_on);
        assert!(is_on(&control, "strip-c").await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_missed_code_is_sent_again() {
        let world = World { plug_on: true, ..Default::default() };
        let (control, world) = hub("strip-d", &["Power"], world).await;
        set_link(&control, "strip-d", Some("plug"), false, Some(1)).await.unwrap();
        learn(&control, "strip-d", "off").await.unwrap();
        learn(&control, "strip-d", "on").await.unwrap();
        control.command("strip-d", "switch", json!({ "on": false })).await.unwrap();
        settle().await;
        /* Missed once: sent again, and it worked. */
        world.lock().unwrap().deaf = 1;
        control.command("strip-d", "switch", json!({ "on": true })).await.unwrap();
        settle().await;
        assert!(world.lock().unwrap().strip_on);
        assert!(is_on(&control, "strip-d").await);
        assert_eq!(config(&control, "strip-d", PROBLEM).await, None);
        /* Missed twice: the measured state, and why. */
        world.lock().unwrap().deaf = 2;
        control.command("strip-d", "switch", json!({ "on": false })).await.unwrap();
        settle().await;
        assert!(world.lock().unwrap().strip_on);
        assert!(is_on(&control, "strip-d").await);
        let problem = config(&control, "strip-d", PROBLEM).await.unwrap();
        assert!(problem.contains("didn't switch off"), "{problem}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_strip_back_on_by_itself_isnt_toggled_off() {
        let world = World { plug_on: true, comes_back_on: true, ..Default::default() };
        let (control, world) = hub("strip-e", &["Power"], world).await;
        set_link(&control, "strip-e", Some("plug"), true, Some(1)).await.unwrap();
        /* It came on with the plug: the person switches it off first
         * (learning step 1 asks them to). */
        world.lock().unwrap().strip_on = false;
        learn(&control, "strip-e", "off").await.unwrap();
        learn(&control, "strip-e", "on").await.unwrap();
        control.command("strip-e", "switch", json!({ "on": false })).await.unwrap();
        assert!(!world.lock().unwrap().plug_on);
        /* Power back: the strip lights up by itself -- measured, so the
         * toggle is NOT pressed (that would switch it off). */
        world.lock().unwrap().presses.clear();
        control.command("strip-e", "switch", json!({ "on": true })).await.unwrap();
        settle().await;
        let w = world.lock().unwrap();
        assert!(w.strip_on, "presses: {:?}", w.presses);
        assert!(w.presses.is_empty(), "presses: {:?}", w.presses);
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_command_stops_the_older_check() {
        let world = World { plug_on: true, ..Default::default() };
        let (control, world) = hub("strip-g", &["On", "Off"], world).await;
        set_link(&control, "strip-g", Some("plug"), false, Some(1)).await.unwrap();
        learn(&control, "strip-g", "off").await.unwrap();
        learn(&control, "strip-g", "on").await.unwrap();
        control.command("strip-g", "switch", json!({ "on": false })).await.unwrap();
        settle().await;
        /* "On" is missed, and "Off" comes right after: the first check
         * must not send "On" again -- the person changed their mind. */
        world.lock().unwrap().deaf = 1;
        world.lock().unwrap().presses.clear();
        control.command("strip-g", "switch", json!({ "on": true })).await.unwrap();
        control.command("strip-g", "switch", json!({ "on": false })).await.unwrap();
        settle().await;
        let w = world.lock().unwrap();
        assert_eq!(w.presses, vec!["On", "Off"]);
        assert!(!w.strip_on);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cloud_plugs_old_reading_isnt_trusted() {
        /* EZVIZ-like: the reading shows a change 20 s late. */
        let world = World { plug_on: true, lag: Duration::from_secs(20), ..Default::default() };
        let (control, world) = hub("strip-h", &["Power"], world).await;
        set_link(&control, "strip-h", Some("plug"), false, Some(1)).await.unwrap();
        /* Learning waits for a fresh reading: real values, not stale. */
        assert_eq!(learn(&control, "strip-h", "off").await.unwrap()["watts"], json!(STANDBY_W));
        assert_eq!(learn(&control, "strip-h", "on").await.unwrap()["watts"], json!(STRIP_ON_W));
        settle().await;
        /* Off with the toggle: the old reading (on) for 20 s must not make
         * the hub press it again (that would switch the strip back on). */
        world.lock().unwrap().presses.clear();
        control.command("strip-h", "switch", json!({ "on": false })).await.unwrap();
        settle().await;
        let w = world.lock().unwrap();
        assert_eq!(w.presses, vec!["Power"]);
        assert!(!w.strip_on);
        drop(w);
        assert!(!is_on(&control, "strip-h").await);
        assert_eq!(config(&control, "strip-h", PROBLEM).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn links_are_checked_and_undone() {
        let (control, _) = hub("strip-f", &["Power"], World::default()).await;
        assert!(set_link(&control, "strip-f", Some("strip-f"), false, None).await.is_err());
        assert!(set_link(&control, "plug", Some("strip-f"), false, None).await.is_err());
        assert!(set_link(&control, "strip-f", Some("nothing"), false, None).await.is_err());
        /* Linked: a switch; unlinked: gone again, config clean. */
        let d = set_link(&control, "strip-f", Some("plug"), false, None).await.unwrap();
        assert!(d.capabilities.switch.is_some());
        let d = set_link(&control, "strip-f", None, false, None).await.unwrap();
        assert!(d.capabilities.switch.is_none() && d.config.is_empty(), "{:?}", d.config);
        /* A removed plug undoes the link. */
        set_link(&control, "strip-f", Some("plug"), false, None).await.unwrap();
        control.remove("plug").await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(config(&control, "strip-f", POWERED_BY).await, None);
    }
}
