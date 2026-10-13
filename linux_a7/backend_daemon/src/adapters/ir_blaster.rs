/*
 * ir_blaster.rs -- the adapter for the hub's own IR blaster add-on (issue
 * #42; template templates/ir-blaster.json; firmware and protocol:
 * firmware_ir_blaster/, PROTOCOL.md there).
 *
 * WHAT IT IS. An ESP32 with IR LEDs and an IR receiver, placed right next
 * to ONE device that has an IR remote (an LED strip, a projector, a fan).
 * In the hub it IS that device: "Astronaut", with a remote whose buttons
 * the person teaches by pressing them on the original remote.
 *
 * THE CONNECTION (shared with the room sensor since #86: addon.rs, which
 * also does the pairing and the Bluetooth setup): mutual TLS, pinned both ways. The hub connects (port
 * 7443) with its own identity certificate (tls.rs) as the client
 * certificate; the blaster checks it against the hub it paired with, the
 * hub checks the blaster's against the fingerprint pinned at pairing
 * (config "fingerprint"). Inside: one JSON object per line.
 *
 * SETUP (the wizard), a new blaster (no WiFi yet):
 *   form             pairing code, WiFi name (the hub's own, pre-filled)
 *                    and password
 *   provision_ble    action "provision" (below): over Bluetooth, the code
 *                    opens an SRP6a session (esp_prov/) and the WiFi goes
 *                    to the blaster; once it's on the WiFi, the same code
 *                    pairs it (as below)
 *   test             connect pinned, "hello" (probe, below)
 *
 * ...or one already on the WiFi:
 *   discover         mDNS _uc-irblaster._tcp -> "host", "blaster_id"
 *   code_from_device the pairing code (action "pair", below): both sides
 *                    prove they know it, without sending it, bound to
 *                    this very TLS connection's certificates -> pin
 *   test             connect pinned, "hello" (probe, below)
 *
 * THE DEVICE'S TASK:
 *   connect   pinned mutual TLS; report the taught buttons + online
 *   session   carry out actions (press -> "send", learn -> "learn"),
 *             ping every PING_EVERY, give up after SILENT_LIMIT of
 *             silence (an unplugged ESP32's connection never closes)
 *   lost      report offline (or "unauthorized" if the blaster refuses
 *             us / shows another certificate: pair again), wait RETRY,
 *             doubling up to RETRY_MAX, try again
 *
 * THE BUTTONS: the `remote` capability with learn: true (device.rs). The
 * codes themselves stay on the hub (ir_codes.rs), keyed by button name;
 * clients only ever see the names. forget / rename only change that store,
 * so they also work while the blaster is offline.
 *
 * THE CODE LIBRARY (issue #82, ir_library.rs), for a device without its
 * remote: library / finder look up code sets, try sends one button of a
 * set, use_set copies a set's buttons into the store (after the taught
 * ones). Only `try` needs the blaster.
 *
 * AS A LIGHT (issue #85, ir_light.rs): an LED strip's device, once marked
 * as a light (action "light", or a set of the library's "LED lights"),
 * also gets `switch` and `color` from the buttons that mean that, and the
 * remote's `brightness` buttons. Their state is ASSUMED (IR goes one way):
 * what the hub last sent, also by pressing those buttons on the remote
 * screen. Config "light" = "on", "colour_order" = "GRB" (device.rs
 * COLOR_ORDERS); both change without restarting the task.
 */
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, oneshot};

use super::addon::{self, read_lines, Kind, Stream, PING_EVERY, REPLY_TIMEOUT, RETRY_MAX, RETRY_MIN, SILENT_LIMIT};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{BrightnessButtons, Color, Device, Health, Remote, Switch};
use crate::templates::ErrorKind;
use crate::ir_encode;
use crate::ir_library::{self, IrLibrary};
use crate::ir_light::{self, LightMap, Role};

/* The blaster's names (addon.rs; the firmware's main.c says the same).
 * The Bluetooth service is the template's provision_ble step's too. */
pub const KIND: Kind = Kind {
    adapter: "ir-blaster",
    product: "IR blaster",
    short: "blaster",
    pair_label: "uc-irb-pair-v1",
    ble_service: "1f2b9c64-3a5e-4c1d-9f0a-5b6e7d8c9a01",
    id_key: "blaster_id",
};

/* How long "learn" waits for a button press unless the client says. */
const LEARN_DEFAULT_S: u64 = 15;
/* Between two buttons of one light command (Power, then a colour): a
 * strip's controller may miss a frame that follows another at once. */
const BETWEEN_PRESSES: Duration = Duration::from_millis(300);
/* The code library's type whose sets make a device a light at once. */
const LIGHT_LIBRARY_TYPE: &str = "led_lighting";

pub struct IrBlaster;

impl Adapter for IrBlaster {
    fn id(&self) -> &'static str {
        "ir-blaster"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            pin: device.config.get("fingerprint").cloned(),
            hub,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle::new(commands)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move { addon::probe(&KIND, values).await.map(|(probe, _)| probe) })
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(async move {
            match name {
                "pair" => addon::pair(&KIND, values).await,
                "provision" => addon::provision(&KIND, values).await,
                other => Err(SetupError::new(ErrorKind::Unsupported, format!("ir-blaster has no action {other:?}"))),
            }
        })
    }
}

/* ------------------------------------------------------------------ */
/* Talking to a blaster, one request at a time (setup)                 */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/* The device's task                                                   */
/* ------------------------------------------------------------------ */

struct Task {
    id: String,
    host: Option<String>,
    /* The blaster's pinned certificate fingerprint (from pairing). */
    pin: Option<String>,
    hub: Hub,
}

/* A request sent, waiting for its reply. */
struct Pending {
    kind: PendingKind,
    deadline: Instant,
}

enum PendingKind {
    Ping,
    Send(oneshot::Sender<Result<Value, String>>),
    Learn {
        button: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

/* One live connection: the write half here, lines read by a separate
 * task (read_lines) and handed over through `incoming`. */
struct Conn {
    writer: WriteHalf<Stream>,
    incoming: mpsc::Receiver<Value>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    last_heard: Instant,
    last_ping: Instant,
}

enum End {
    /* The device was removed (its command channel closed). */
    Stop,
    /* The connection broke. */
    Lost(String),
}

impl Task {
    async fn run(self, mut commands: mpsc::Receiver<DeviceCmd>) {
        /* The taught buttons (and what they do for a light) are known
         * even before the blaster answers. */
        self.sync_light().await;
        let (Some(host), Some(pin)) = (self.host.clone(), self.pin.clone()) else {
            println!("ir-blaster: {} has no address or pairing: pair it again", self.id);
            self.hub.set_online(&self.id, Health::Unauthorized).await;
            while let Some(cmd) = commands.recv().await {
                self.offline_command(cmd).await;
            }
            return;
        };

        let mut retry = RETRY_MIN;
        loop {
            match addon::open(&host, Some(&pin)).await {
                Ok((stream, _)) => {
                    retry = RETRY_MIN;
                    let (reader, writer) = tokio::io::split(stream);
                    let (tx, incoming) = mpsc::channel(8);
                    tokio::spawn(read_lines(reader, tx));
                    let conn = Conn {
                        writer,
                        incoming,
                        pending: HashMap::new(),
                        next_id: 1,
                        last_heard: Instant::now(),
                        last_ping: Instant::now(),
                    };
                    self.hub.set_online(&self.id, Health::Online).await;
                    match self.session(conn, &mut commands).await {
                        End::Stop => return,
                        End::Lost(why) => println!("ir-blaster: {}: connection lost: {why}", self.id),
                    }
                    self.hub.set_online(&self.id, Health::Offline).await;
                }
                Err(e) => {
                    /* Refused: the blaster shows another certificate, or
                     * closes on us (it was reset / paired elsewhere). */
                    let health = if e.kind == ErrorKind::Refused { Health::Unauthorized } else { Health::Offline };
                    self.hub.set_online(&self.id, health).await;
                }
            }

            /* Wait before the next attempt, answering commands meanwhile.
             * A press or learn tries again at once (the person wants it
             * now, and the blaster may be back). */
            let until = tokio::time::Instant::now() + retry;
            loop {
                tokio::select! {
                    cmd = commands.recv() => match cmd {
                        None => return,
                        Some(cmd) => {
                            if self.offline_command(cmd).await {
                                break;
                            }
                        }
                    },
                    _ = tokio::time::sleep_until(until) => break,
                }
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /* A command while there's no connection. Returns true if it needed
     * the blaster (so: try to reconnect now). */
    async fn offline_command(&self, cmd: DeviceCmd) -> bool {
        match cmd {
            DeviceCmd::Action { capability, name, args, reply } if capability == "remote" && is_hub_only(&name) => {
                let _ = reply.send(self.hub_only_action(&name, &args).await);
                false
            }
            cmd => {
                cmd.refuse("The IR blaster isn't reachable right now. Is it plugged in and on the WiFi?");
                true
            }
        }
    }

    /* The remote actions that only need the hub's own data -- its stored
     * buttons (ir_codes.rs) and the code library (ir_library.rs) -- so
     * they work while the blaster is offline too. */
    async fn hub_only_action(&self, name: &str, args: &Value) -> Result<Value, String> {
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
        match name {
            "forget" | "rename" => {
                let button = text("button");
                match name {
                    "forget" => self.hub.ir_codes().forget(&self.id, &button)?,
                    _ => self.hub.ir_codes().rename(&self.id, &button, &text("to"))?,
                };
                self.sync_light().await;
                Ok(json!({}))
            }
            "library" => {
                let type_id = args.get("type").map(|_| text("type"));
                library(move |lib| match type_id {
                    None => Ok(json!({"types": lib.types()?})),
                    Some(type_id) => Ok(json!({"brands": lib.brands(&type_id)?})),
                })
                .await
            }
            "finder" => {
                let (type_id, brand) = (text("type"), text("brand"));
                library(move |lib| Ok(json!({"candidates": lib.candidates(&type_id, &brand)?}))).await
            }
            "use_set" => {
                let (type_id, set_id) = (text("type"), text("set"));
                let set = library(move |lib| lib.set(&type_id, &set_id)).await?;
                /* Only what this hub can send: a button it can't would
                 * just fail when pressed. Stored as the library has it
                 * ({"proto": "rc5", ...} is a few bytes, its raw form
                 * hundreds) and encoded at every press (ir_encode.rs:
                 * RC5/RC6 change a bit on every press). */
                let buttons: Vec<(String, Value)> =
                    set.buttons.into_iter().filter(|(_, code)| ir_encode::to_blaster(code).is_some()).collect();
                /* A TV's buttons by the hub's standard names: the screen
                 * then lays them out as a remote. */
                let buttons = ir_library::standard_names(&text("type"), buttons);
                let (_, added) = self.hub.ir_codes().add_set(&self.id, buttons);
                /* An LED strip's set: it's a light (issue #85) -- unless
                 * the person already said it isn't. */
                if text("type") == LIGHT_LIBRARY_TYPE && self.light_setting("light").await.is_none() {
                    self.hub.store_config(&self.id, "light", "on").await;
                }
                self.sync_light().await;
                println!("ir-blaster: {}: {added} buttons from the library ({})", self.id, set.id);
                Ok(json!({"added": added}))
            }
            "light" => {
                let enabled = args["enabled"].as_bool().unwrap_or(false);
                self.hub.store_config(&self.id, "light", if enabled { "on" } else { "off" }).await;
                if let Some(order) = args["order"].as_str() {
                    self.hub.store_config(&self.id, "colour_order", order).await;
                }
                let map = self.sync_light().await;
                Ok(json!({
                    "switch": enabled && map.can_switch(),
                    "colors": if enabled { map.colors.len() } else { 0 },
                    "brightness": enabled && map.has_brightness(),
                }))
            }
            other => Err(format!("ir-blaster can't {other} without the blaster")),
        }
    }

    /* One of the device's config values, read now: the "light" action
     * changes them while the task runs. */
    async fn light_setting(&self, key: &str) -> Option<String> {
        self.hub.device(&self.id).await?.config.get(key).cloned()
    }

    /* The colour order if the device is used as a light, None if not. */
    async fn light_order(&self) -> Option<String> {
        let device = self.hub.device(&self.id).await?;
        if device.config.get("light").map(String::as_str) != Some("on") {
            return None;
        }
        Some(device.config.get("colour_order").cloned().unwrap_or_else(|| "RGB".into()))
    }

    /* Reports the buttons, and makes the device's light capabilities
     * match them (issue #85): `switch` if they can switch it on and off,
     * `color` (with the palette) if there are colour buttons, the
     * remote's brightness buttons -- or none of those if it isn't used as
     * a light. After every change to the buttons or the settings. */
    async fn sync_light(&self) -> LightMap {
        let buttons = self.hub.ir_codes().get(&self.id);
        let map = LightMap::from_buttons(&buttons);
        let order = self.light_order().await;
        let light = order.is_some();
        let brightness = match (&map.brighter, &map.dimmer) {
            (Some(up), Some(down)) if light => Some(BrightnessButtons {
                up: up.clone(),
                down: down.clone(),
            }),
            _ => None,
        };
        let names = buttons.into_iter().map(|b| b.button).collect();
        report_buttons(&self.hub, &self.id, names, brightness).await;

        let mut want: Vec<&str> = Vec::new();
        let mut drop: Vec<&str> = Vec::new();
        /* Powered by a plug (issue #99): it keeps its `switch` whatever
         * its buttons -- the plug is its power (power_link.rs). */
        let powered = self.light_setting(crate::power_link::POWERED_BY).await.is_some();
        (if (light && map.can_switch()) || powered { &mut want } else { &mut drop }).push("switch");
        (if light && !map.colors.is_empty() { &mut want } else { &mut drop }).push("color");
        if let Err(e) = self.hub.add_capabilities(&self.id, &want).await {
            println!("ir-blaster: {}: {e}", self.id);
        }
        if let Err(e) = self.hub.remove_capabilities(&self.id, &drop).await {
            println!("ir-blaster: {}: {e}", self.id);
        }
        if let Some(order) = &order {
            if !map.colors.is_empty() {
                /* The palette, and a current colour that's in it (the one
                 * the hub last sent, if it still is). */
                let palette = map.palette(order);
                let current = self.hub.device(&self.id).await.and_then(|d| d.capabilities.color).and_then(|c| c.hex);
                let hex = current.filter(|h| palette.contains(h)).unwrap_or_else(|| palette[0].clone());
                self.report_color(hex, palette).await;
            }
        }
        map
    }

    async fn report_color(&self, hex: String, palette: Vec<String>) {
        let color = Color {
            hex: Some(hex),
            kelvin: None,
            palette,
        };
        if let Err(e) = self.hub.report(&self.id, "color", json!(color)).await {
            println!("ir-blaster: {}: {e}", self.id);
        }
    }

    async fn report_switch(&self, on: bool) {
        if let Err(e) = self.hub.report(&self.id, "switch", json!(Switch { on })).await {
            println!("ir-blaster: {}: {e}", self.id);
        }
    }

    /* What a pressed button did to the light, as far as the hub can tell
     * (issue #85): a Power toggle flips the assumed state, a colour button
     * sets the colour. Nothing if it isn't used as a light. */
    async fn pressed(&self, button: &str) {
        let Some(order) = self.light_order().await else { return };
        let map = LightMap::from_buttons(&self.hub.ir_codes().get(&self.id));
        let Some(device) = self.hub.device(&self.id).await else { return };
        let assumed_on = device.capabilities.switch.as_ref().is_some_and(|s| s.on);
        match map.role(button) {
            Some(Role::On) if map.can_switch() => self.report_switch(true).await,
            Some(Role::Off) if map.can_switch() => self.report_switch(false).await,
            Some(Role::Toggle) => self.report_switch(!assumed_on).await,
            Some(Role::Color(rgb)) => {
                self.report_color(ir_light::to_hex(ir_light::shown_color(rgb, &order)), map.palette(&order)).await;
            }
            _ => {}
        }
    }

    /* Sends one button now and waits for the blaster's answer, handling
     * whatever else arrives meanwhile (a light command presses up to two
     * buttons, in order). The outer Err: the connection broke; the inner
     * one: the blaster couldn't send it. */
    async fn press_now(&self, conn: &mut Conn, button: &str) -> Result<Result<(), String>, String> {
        let Some(code) = self.hub.ir_codes().code(&self.id, button) else {
            return Ok(Err(format!("{button:?} isn't taught yet")));
        };
        let Some(code) = ir_encode::for_sending(&code) else {
            return Ok(Err(format!("this hub can't send {button:?} ({})", code["proto"])));
        };
        let (tx, mut rx) = oneshot::channel();
        send_request(conn, "send", json!({"code": code}), PendingKind::Send(tx), REPLY_TIMEOUT).await?;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                answer = &mut rx => {
                    return Ok(answer.unwrap_or_else(|_| Err("The IR blaster didn't answer.".into())).map(|_| ()));
                }
                msg = conn.incoming.recv() => match msg {
                    None => return Err("the blaster closed the connection".into()),
                    Some(msg) => {
                        conn.last_heard = Instant::now();
                        self.message(conn, msg).await;
                    }
                },
                _ = tick.tick() => expire(conn),
            }
        }
    }

    /* A `switch` or `color` command to a device used as a light (issue
     * #85): the buttons that do it, then the assumed new state. */
    async fn light_command(&self, conn: &mut Conn, capability: &str, value: &Value) -> Result<Result<(), String>, String> {
        let Some(order) = self.light_order().await else {
            return Ok(Err("this IR device isn't used as a light".into()));
        };
        let map = LightMap::from_buttons(&self.hub.ir_codes().get(&self.id));
        let assumed_on = self
            .hub
            .device(&self.id)
            .await
            .and_then(|d| d.capabilities.switch)
            .is_some_and(|s| s.on);
        let mut presses: Vec<String> = Vec::new();
        let (on, color) = match capability {
            "switch" => {
                let on = value["on"].as_bool().unwrap_or(false);
                presses = map.presses_for_switch(on, assumed_on);
                (Some(on), None)
            }
            "color" => {
                let want = match (value["hex"].as_str(), value["kelvin"].as_u64()) {
                    (Some(hex), _) => ir_light::from_hex(hex),
                    (None, Some(k)) => Some(ir_light::kelvin_to_rgb(k.min(u16::MAX as u64) as u16)),
                    _ => None,
                };
                let Some((button, shown)) = want.and_then(|rgb| map.nearest(rgb, &order)) else {
                    return Ok(Err("this light has no colour buttons".into()));
                };
                /* A strip that's off ignores colour buttons: on first. */
                let mut on = None;
                if !assumed_on && map.can_switch() {
                    presses = map.presses_for_switch(true, false);
                    on = Some(true);
                }
                presses.push(button);
                (on, Some(ir_light::to_hex(shown)))
            }
            other => return Ok(Err(format!("an IR light has no {other} to set"))),
        };
        for (i, button) in presses.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(BETWEEN_PRESSES).await;
            }
            if let Err(why) = self.press_now(conn, button).await? {
                return Ok(Err(why));
            }
        }
        if let Some(on) = on {
            self.report_switch(on).await;
        }
        if let Some(hex) = color {
            self.report_color(hex, map.palette(&order)).await;
        }
        Ok(Ok(()))
    }

    async fn session(&self, mut conn: Conn, commands: &mut mpsc::Receiver<DeviceCmd>) -> End {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let end = loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    None => break End::Stop,
                    Some(cmd) => {
                        if let Err(why) = self.command(&mut conn, cmd).await {
                            break End::Lost(why);
                        }
                    }
                },
                msg = conn.incoming.recv() => match msg {
                    None => break End::Lost("the blaster closed the connection".into()),
                    Some(msg) => {
                        conn.last_heard = Instant::now();
                        self.message(&mut conn, msg).await;
                    }
                },
                _ = tick.tick() => {
                    if conn.last_heard.elapsed() > SILENT_LIMIT {
                        break End::Lost(format!("silent for {} s", SILENT_LIMIT.as_secs()));
                    }
                    if conn.last_ping.elapsed() >= PING_EVERY {
                        conn.last_ping = Instant::now();
                        if let Err(why) = send_request(&mut conn, "ping", json!({}), PendingKind::Ping, REPLY_TIMEOUT).await {
                            break End::Lost(why);
                        }
                    }
                    expire(&mut conn);
                }
            }
        };
        /* Whoever still waits for an answer gets one. */
        for (_, pending) in conn.pending.drain() {
            fail(pending.kind, "The connection to the IR blaster was lost. Try again.");
        }
        end
    }

    /* A command or action from control.rs (already checked by device.rs).
     * Err only if the connection broke while sending. */
    async fn command(&self, conn: &mut Conn, cmd: DeviceCmd) -> Result<(), String> {
        let (capability, name, args, reply) = match cmd {
            DeviceCmd::Action { capability, name, args, reply } => (capability, name, args, reply),
            DeviceCmd::Command { capability, value, reply } => {
                /* Only a light's switch and colour (issue #85). */
                let result = self.light_command(conn, &capability, &value).await;
                let broken = result.as_ref().err().cloned();
                let _ = reply.send(result.unwrap_or_else(|_| Err("The connection to the IR blaster was lost. Try again.".into())));
                return broken.map_or(Ok(()), Err);
            }
        };
        if capability != "remote" {
            let _ = reply.send(Err(format!("ir-blaster has no {capability} actions")));
            return Ok(());
        }
        match name.as_str() {
            "press" => {
                /* Taught codes pass through; library ones are encoded
                 * (press_now). Waited for: a light's assumed state
                 * (issue #85) only changes once it was sent. */
                let button = args["button"].as_str().unwrap_or_default();
                match self.press_now(conn, button).await {
                    Ok(Ok(())) => {
                        self.pressed(button).await;
                        let _ = reply.send(Ok(json!({})));
                        Ok(())
                    }
                    Ok(Err(why)) => {
                        let _ = reply.send(Err(why));
                        Ok(())
                    }
                    Err(broken) => {
                        let _ = reply.send(Err("The connection to the IR blaster was lost. Try again.".into()));
                        Err(broken)
                    }
                }
            }
            "learn" => {
                let seconds = args["timeout_s"].as_u64().unwrap_or(LEARN_DEFAULT_S);
                let kind = PendingKind::Learn {
                    button: args["button"].as_str().unwrap_or_default().to_string(),
                    reply,
                };
                /* The blaster answers "timeout" itself; our own limit is
                 * only for a blaster that never answers. */
                let limit = Duration::from_secs(seconds) + REPLY_TIMEOUT;
                send_request(conn, "learn", json!({"timeout_ms": seconds * 1000}), kind, limit).await
            }
            "try" => {
                /* One button of a library set, for the code finder: sent,
                 * not stored. */
                let (type_id, set_id, button) = (
                    args["type"].as_str().unwrap_or_default().to_string(),
                    args["set"].as_str().unwrap_or_default().to_string(),
                    args["button"].as_str().unwrap_or_default().to_string(),
                );
                let code = library(move |lib| {
                    let set = lib.set(&type_id, &set_id)?;
                    let (_, code) = set.buttons.iter().find(|(name, _)| *name == button).ok_or_else(|| format!("{set_id} has no button {button:?}"))?;
                    ir_encode::for_sending(code).ok_or_else(|| format!("this hub can't send {}", code["proto"]))
                })
                .await;
                match code {
                    Ok(code) => send_request(conn, "send", json!({"code": code}), PendingKind::Send(reply), REPLY_TIMEOUT).await,
                    Err(why) => {
                        let _ = reply.send(Err(why));
                        Ok(())
                    }
                }
            }
            name if is_hub_only(name) => {
                let _ = reply.send(self.hub_only_action(name, &args).await);
                Ok(())
            }
            other => {
                let _ = reply.send(Err(format!("ir-blaster can't {other}")));
                Ok(())
            }
        }
    }

    /* A line from the blaster: a reply to one of our requests, or an
     * event. */
    async fn message(&self, conn: &mut Conn, msg: Value) {
        let Some(id) = msg["id"].as_u64() else {
            /* "heard": the original remote was used. Later (#42 DoD) this
             * updates the device's state; for now it's only logged. */
            if msg["event"] == json!("heard") {
                println!("ir-blaster: {}: remote pressed: {}", self.id, msg["code"]);
            }
            return;
        };
        let Some(pending) = conn.pending.remove(&id) else { return };
        if msg["ok"] != json!(true) {
            let why = match msg["error"].as_str() {
                Some("timeout") => "No button was pressed on the remote in time. Try again.".to_string(),
                Some(error) => format!("The IR blaster said: {error} ({})", msg["detail"].as_str().unwrap_or("")),
                None => "The IR blaster sent a strange answer".to_string(),
            };
            fail(pending.kind, &why);
            return;
        }
        match pending.kind {
            PendingKind::Ping => {}
            PendingKind::Send(reply) => {
                let _ = reply.send(Ok(json!({})));
            }
            PendingKind::Learn { button, reply } => {
                let code = msg["code"].clone();
                if !code.is_object() {
                    let _ = reply.send(Err("The IR blaster sent no code".into()));
                    return;
                }
                self.hub.ir_codes().learn(&self.id, &button, code.clone());
                self.sync_light().await;
                println!("ir-blaster: {}: learned {button:?}: {code}", self.id);
                let _ = reply.send(Ok(json!({"code": code})));
            }
        }
    }
}

async fn report_buttons(hub: &Hub, id: &str, buttons: Vec<String>, brightness: Option<BrightnessButtons>) {
    let remote = Remote {
        buttons,
        keyboard: false,
        learn: true,
        brightness,
    };
    if let Err(e) = hub.report(id, "remote", json!(remote)).await {
        println!("ir-blaster: {id}: {e}");
    }
}

/* Sends a request and remembers who waits for the reply. Err if the
 * connection broke. */
async fn send_request(conn: &mut Conn, op: &str, fields: Value, kind: PendingKind, limit: Duration) -> Result<(), String> {
    let id = conn.next_id;
    conn.next_id += 1;
    let mut msg = json!({"id": id, "op": op});
    if let (Some(msg), Some(fields)) = (msg.as_object_mut(), fields.as_object()) {
        msg.extend(fields.clone());
    }
    let mut line = msg.to_string();
    line.push('\n');
    if let Err(e) = conn.writer.write_all(line.as_bytes()).await {
        fail(kind, "The connection to the IR blaster was lost. Try again.");
        return Err(e.to_string());
    }
    conn.pending.insert(
        id,
        Pending {
            kind,
            deadline: Instant::now() + limit,
        },
    );
    Ok(())
}

/* Requests whose reply is overdue get an error. */
fn expire(conn: &mut Conn) {
    let now = Instant::now();
    let overdue: Vec<u64> = conn
        .pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in overdue {
        if let Some(pending) = conn.pending.remove(&id) {
            fail(pending.kind, "The IR blaster didn't answer in time.");
        }
    }
}

fn fail(kind: PendingKind, why: &str) {
    match kind {
        PendingKind::Ping => {}
        PendingKind::Send(reply) | PendingKind::Learn { reply, .. } => {
            let _ = reply.send(Err(why.to_string()));
        }
    }
}

/* Remote actions the hub answers from its own data (hub_only_action). */
fn is_hub_only(name: &str) -> bool {
    matches!(name, "forget" | "rename" | "library" | "finder" | "use_set" | "light")
}

/* Runs a code library lookup on a blocking thread: reading and parsing a
 * type file (up to ~1 MB) takes a moment on the board, and the async
 * threads that serve every device and client must not wait for it. */
async fn library<T: Send + 'static>(f: impl FnOnce(&IrLibrary) -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(move || f(ir_library::shared()))
        .await
        .map_err(|e| format!("IR code library: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Registry;
    use crate::control::Control;
    use crate::device::{Capabilities, Source};
    use crate::secrets::{Secret, Secrets};
    use crate::state::{self, Event, Outputs};
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

    /* (The pairing code and proof tests moved to addon.rs with the code,
     * #86.) */

    /* ---- against a real blaster on the bench ----
     *
     *   HUB_TLS_DIR=/tmp/irb-test-hub IRB_HOST=192.168.1.143 \
     *   IRB_CODE=XXXX-XXXX-XXXX-XXXX cargo test ir_blaster::tests::live -- --ignored --nocapture
     *
     * The blaster must be unpaired first. Pairs through the wizard's
     * action, runs the test step, then a device task: learn (press a
     * button on a remote within 60 s when asked), press it back, rename,
     * forget -- and unpairs at the end, leaving the blaster free. */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let host = std::env::var("IRB_HOST").expect("set IRB_HOST");
        let code = std::env::var("IRB_CODE").expect("set IRB_CODE");

        let mut values = SetupValues::default();
        values.plain.insert("host".into(), host.clone());
        values.secret.insert("pairing_code".into(), Secret::new(code));
        let paired = addon::pair(&KIND, &values).await.expect("pairing");
        println!("paired: {:?}", paired.plain);
        values.plain.extend(paired.plain.clone());
        let (probe, _) = addon::probe(&KIND, &values).await.expect("test step");
        println!("test step: {}", probe.summary);

        /* The hub's parts a device task talks to. */
        let device = Device {
            id: "astro".into(),
            name: "Astronaut".into(),
            room: String::new(),
            template: "ir-blaster".into(),
            source: Source::new("ir-blaster"),
            config: values.plain.clone(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: Capabilities::with_defaults(&["remote".into()]).unwrap(),
        };
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, mut events) = broadcast::channel(64);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, [(device.id.clone(), device)].into(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(IrBlaster)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;

        /* Online within a few seconds. */
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if control.get("astro").await.unwrap().unwrap().online == Some(Health::Online) {
                break;
            }
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(Event::Changed(_))) | Ok(Ok(_)) => {}
                Ok(Err(_)) => {}
                Err(_) => panic!("never came online"),
            }
        }
        println!("online");

        println!(">>> PRESS A BUTTON ON THE REMOTE NOW (60 s) <<<");
        let learned = control
            .action("astro", "remote", "learn", json!({"button": "Power", "timeout_s": 60}))
            .await
            .expect("learn");
        println!("learned: {learned}");
        let d = control.get("astro").await.unwrap().unwrap();
        assert_eq!(d.capabilities.remote.as_ref().unwrap().buttons, vec!["Power"]);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let started = Instant::now();
        control.action("astro", "remote", "press", json!({"button": "Power"})).await.expect("press");
        println!("pressed Power: {} ms", started.elapsed().as_millis());

        control
            .action("astro", "remote", "rename", json!({"button": "Power", "to": "On/Off"}))
            .await
            .expect("rename");
        /* A pause a person can see: IR receivers (LED strips especially)
         * ignore the same code repeated within a fraction of a second. */
        tokio::time::sleep(Duration::from_secs(3)).await;
        control.action("astro", "remote", "press", json!({"button": "On/Off"})).await.expect("press renamed");
        println!("pressed On/Off (renamed) -- the device should be back as it was");
        control.action("astro", "remote", "forget", json!({"button": "On/Off"})).await.expect("forget");
        let d = control.get("astro").await.unwrap().unwrap();
        assert!(d.capabilities.remote.as_ref().unwrap().buttons.is_empty());

        /* Leave the blaster unpaired. */
        control.remove("astro").await.unwrap();
        let (mut link, _) = addon::setup_link(&KIND, &host, Some(&values.plain["fingerprint"])).await.unwrap();
        println!("unpair: {}", link.request("unpair", json!({})).await.unwrap());
    }
}
