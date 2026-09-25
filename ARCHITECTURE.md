# A7 Software Architecture

Status: living document for the Sprint 2/3 plan (GitHub issues #9-#18). Written
2026-09-22. Supersedes nothing on disk (no prior `ARCHITECTURE.md` exists in
this repo — the original one, describing a `hubd`/web-UI design, was
discarded along with that code on 2026-09-16).

## Scope

Device support is general-purpose, not tied to any one device category
(camera, vacuum, TV, or otherwise). No device-adapter work is scheduled yet;
this document covers the platform/plumbing layer that adapters will plug
into later.

## Processes

Two independent Rust processes run on the A7 (Cortex-A7, dual-core,
650-800 MHz per `cpu0_opp_table` in the kernel DT, 1 GB DDR), matching the
Makefile's `build-a7` target:

- **`linux_a7/backend_daemon`** — the hub. Owns every external connection
  (devices, mobile/LAN clients, cloud, the M4) and all application state.
  Async, built on Tokio.
- **`linux_a7/ui_layer`** — the hub's GUI. Built with Slint, drawing through
  DRM/KMS (`/dev/dri/card0`) on the touchscreen, or on an HDMI monitor when
  one is connected (issue #38). Deliberately "dumb": displays whatever state the
  daemon gives it, forwards touch input back. Holds no state of its own.

Both are empty placeholder directories as of 2026-09-22; Task 8 is the
current work (bootstrapping `backend_daemon`).

## `backend_daemon` internals

```
main.rs      — runtime bootstrap: spawn every subsystem below, then await
               a shutdown signal (SIGTERM from systemd / Ctrl-C)
state.rs     — the hub-state actor (see "State ownership" below)
ws.rs        — Axum WebSocket server (Task 11) — serves both LAN/mobile
               clients AND ui_layer (see "Local IPC" below)
mqtt.rs      — Rumqttc MQTT client (Task 13) — cloud device-twin sync
rpmsg.rs     — RPMsg link to the M4 (Task 10)
```

Each device connection (once device-adapter work exists) is its own
`tokio::spawn`ed task, holding a cloned handle into the state actor's
channel — not a thread, not a Mutex-guarded shared reference.

### Runtime config

`#[tokio::main(worker_threads = 2)]` — set explicitly rather than relying on
Tokio's auto-detect. It happens to match the physical core count either way,
but pinning it documents the intent instead of it looking accidental.

### State ownership: actor, not Mutex

`state.rs` owns `HubState` exclusively. Every other task (device tasks,
`ws.rs`, `mqtt.rs`, `rpmsg.rs`) holds a cloned `mpsc::Sender<Msg>` and only
ever *sends messages* to request a read or write — never touches `HubState`
directly. Reads that need a reply carry a `oneshot::Sender` in the message.

Chosen over `Arc<Mutex<HubState>>` because:
- No lock ever needs to be held across an `.await`, so no risk of one slow
  task stalling every other task that touches state.
- No deadlock surface — there's only ever one owner.
- The message enum is a self-documenting list of everything anyone is
  allowed to do to hub state.

Chosen as *one* actor for all hub state, not one actor per device: total
memory cost is the same either way, but N actors would mean N extra tasks
and N extra channels for no benefit at this scale (tens of devices, not
thousands).

`Semaphore` is reserved for a different, later problem — bounding
concurrent CPU-bound work (e.g. capping simultaneous video decode
operations, relevant because the STM32MP157 has no hardware video
decoder — see below). It is not a mechanism for sharing state and isn't
part of this design today.

### CPU-bound work

Tokio's scheduler is cooperative: nothing preempts a task that's actually
computing, not even something more urgent. Anything CPU-heavy (decode,
hashing, heavy parsing) must go through `tokio::task::spawn_blocking`,
which runs it on Tokio's separate blocking-thread pool instead of the 2
async worker threads, so it can't starve device I/O tasks or the RPMsg
link. This matters more than usual here: the STM32MP157 has no hardware
video decoder (Vivante GPU only, no VPU), so any future on-device
decode work is a real CPU cost, not a rounding error.

## Local IPC: `ui_layer` ↔ `backend_daemon`

`ui_layer` connects to `ws.rs`'s Axum WebSocket server as an ordinary local
client (`ws://127.0.0.1:PORT`), the same protocol a phone or LAN browser
client uses — not a separate Unix-socket/custom-framing protocol.

Rationale: one server implementation to build and maintain instead of two,
and the touchscreen and any mobile client see the identical state-update
stream by construction, with no separate sync logic required.

## M4 firmware (`firmware_m4/`)

Built on Zephyr from the start (Task 9, decided 2026-09-22 — supersedes an
earlier FreeRTOS-first/Zephyr-later staging plan, to avoid bringing up
OpenAMP/RPMsg twice). Talks to `backend_daemon` only via RPMsg (Task 10) —
no other coupling.

This side uses a genuinely different concurrency model than the A7, on
purpose:

- **Preemptive, priority-based scheduling** (real context switches via a
  timer tick), not cooperative — a real RTOS, not a superloop.
- **Dedicated, statically-sized stack per task**, not heap-allocated task
  state — avoid dynamic allocation on this side entirely (non-deterministic
  timing, fragmentation risk, minimal RAM — standard embedded practice).

This is intentional, not an inconsistency: anything needing hard,
guaranteed real-time behavior belongs on the M4, which is the whole reason
this SoC has a separate Cortex-M4 core in the first place. `backend_daemon`
is explicitly *not* the hard-real-time path — it's general I/O-bound
application logic, which is exactly what Tokio's cooperative model suits.

## Open / deliberately deferred

- Device-adapter design (camera, vacuum, TV, etc.) — no issue filed yet,
  general-purpose by intent, not designed here.
- Graceful shutdown propagation (telling spawned subsystems to stop
  cleanly, e.g. via a `broadcast` shutdown channel) — not needed until
  `backend_daemon` has real subsystems to shut down.
