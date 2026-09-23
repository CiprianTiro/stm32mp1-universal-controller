/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * M4 firmware: LED control over RPMsg (issue #14)
 * ===========================================================================
 *
 * This file used to also blink an LED and print a heartbeat once a second
 * (issue #11's "Hello World" stage). That's gone now -- the GUI (issue #14)
 * wants the LED to reflect an actual on/off command from the A7 side, not
 * blink on its own, so a free-running timer loop would just get in the way.
 * What's left is purely *reactive*: this firmware does nothing at all until
 * a message arrives over RPMsg, then acts on it and replies.
 *
 * Two subsystems are involved, and it's worth being clear about which is
 * which before reading the code below:
 *
 *   1. remoteproc / OpenAMP plumbing (platform_init, platform_create_rpmsg_vdev,
 *      the mailbox glue) -- this is how the A7 boots this firmware into the
 *      M4's RAM and sets up a shared-memory channel between the two cores.
 *      None of this is specific to LEDs; it's the same machinery any future
 *      RPMsg-based feature on this board would reuse. Copied near-verbatim
 *      from Zephyr's own samples/subsys/ipc/openamp_rsc_table sample, which
 *      is the only upstream sample with board files proven for this exact
 *      board (stm32mp157c_dk2) -- see the GitHub wiki's "M4-Firmware" page
 *      for the full story, including a nasty bug that hid in this exact code
 *      the first time it was adapted (a silent IPM payload-size rejection).
 *
 *   2. The LED command protocol itself (rpmsg_recv_raw_callback,
 *      rpmsg_raw_thread_entry) -- this is the actual "feature" code: parse
 *      a text command, drive a GPIO, reply with the result. Much simpler,
 *      and the part you'd change if e.g. a second LED or button got added.
 */

#include <zephyr/kernel.h>       /* core Zephyr types: threads, semaphores, k_msleep, etc. */
#include <zephyr/device.h>       /* struct device, device_is_ready() -- Zephyr's generic driver-handle API */
#include <zephyr/drivers/gpio.h> /* GPIO_DT_SPEC_GET, gpio_pin_configure_dt, gpio_pin_set_dt */
#include <zephyr/drivers/ipm.h>  /* Inter-Processor Mailbox API -- how the two cores interrupt each other */
#include <zephyr/sys/printk.h>   /* printk() -- goes to the RAM console, readable from Linux's debugfs */
#include <stdio.h>               /* snprintf() -- used to build the reply text */
#include <string.h>              /* strcmp(), memcpy() */

/* OpenAMP is the library that actually implements the RPMsg protocol (message
 * framing, virtqueues, endpoints) on top of the raw shared-memory region and
 * the IPM mailbox below. "metal" is OpenAMP's small portability layer
 * (memory-mapped I/O helpers); resource_table.h/addr_translation.h come from
 * Zephyr's own subsys/ipc/open-amp/ glue, not from this project. */
#include <openamp/open_amp.h>
#include <metal/sys.h>
#include <metal/io.h>
#include <resource_table.h>
#include <addr_translation.h>

/* ===========================================================================
 * Section: constants and the one GPIO this firmware drives
 * ===========================================================================
 */

/* "rpmsg-raw" is not an arbitrary name -- it's the specific RPMsg channel
 * name that Linux's built-in rpmsg_char kernel driver auto-binds to
 * (see drivers/rpmsg/rpmsg_char.c's rpmsg_chrdev_id_table on the Linux side).
 * Using this exact name means a plain /dev/rpmsgN character device shows up
 * on Linux automatically, with no custom kernel driver needed -- the A7 side
 * (linux_a7/backend_daemon/src/rpmsg.rs) just opens that file and does
 * ordinary read()/write() calls on it. */
#define RPMSG_RAW_CHANNEL_NAME "rpmsg-raw"

/* Maximum size of one incoming RPMsg message we'll accept. Commands here are
 * tiny ("LED ON" is 6 bytes), so this is generous headroom, not a tight fit. */
#define RPMSG_RX_BUF_SIZE 256

/* This resolves, at compile time, to the GPIO pin wired to LD7 on the DK2
 * board -- the mapping itself lives in the board's devicetree
 * (boards/st/stm32mp157c_dk2/stm32mp157c_dk2.dts, under "aliases { led0 = ... }"),
 * not here. GPIO_DT_SPEC_GET() is a macro that reads that devicetree node
 * and produces a `struct gpio_dt_spec` -- basically "which GPIO controller,
 * which pin number, and which electrical polarity counts as ON" -- as a
 * compile-time constant, so there's no runtime parsing involved. */
static const struct gpio_dt_spec led = GPIO_DT_SPEC_GET(DT_ALIAS(led0), gpios);

/* Tracks whether the LED is currently on, so "LED STATUS" can answer without
 * having to read the GPIO hardware back (and so the very first command after
 * boot has a known, defined starting point: off). */
static bool led_is_on;

/* ===========================================================================
 * Section: threads
 * ===========================================================================
 * Two Zephyr threads run here (see the GitHub wiki's "M4-Firmware" page for
 * a fuller explanation of why Zephyr threads are real preemptible OS threads,
 * not the cooperative "task" concept Rust's Tokio uses on the A7 side):
 *
 *   - rpmsg_mng_task: does the one-time OpenAMP/vdev setup, then spends the
 *     rest of its life waiting for "the M4 got a mailbox interrupt" and
 *     pumping that into OpenAMP's virtqueue processing. It does NOT know or
 *     care what the messages actually say.
 *   - rpmsg_raw_thread_entry: owns the actual "rpmsg-raw" endpoint. Its
 *     receive callback (rpmsg_recv_raw_callback) just copies the incoming
 *     bytes and wakes this thread up; this thread is where the LED command
 *     text actually gets parsed and acted on.
 *
 * They're kept as two separate threads (rather than doing everything in one)
 * so that if command handling ever got slow for some reason, it couldn't
 * stall the low-level mailbox-notify pump that any *other* future RPMsg
 * endpoint would also depend on.
 */

K_THREAD_STACK_DEFINE(rpmsg_mng_stack, 1024);
K_THREAD_STACK_DEFINE(rpmsg_raw_stack, 1024);
static struct k_thread rpmsg_mng_thread_data;
static struct k_thread rpmsg_raw_thread_data;

/* ===========================================================================
 * Section: OpenAMP/remoteproc state
 * ===========================================================================
 * Everything below this point until "Section: LED command handling" is the
 * generic RPMsg transport plumbing described above -- it doesn't know
 * anything about LEDs. If you're trying to understand the LED feature
 * specifically, skip ahead to that section; come back here if you need to
 * understand *how messages get here at all*.
 */

/* DT_CHOSEN(zephyr_ipc) resolves to whichever devicetree node the board
 * overlay (boards/stm32mp157c_dk2.overlay) designated as "the mailbox" via
 * its `chosen { zephyr,ipc = &mailbox; }` line. DEVICE_DT_GET() turns that
 * devicetree reference into a `const struct device *` handle we can actually
 * call driver functions on at runtime. */
static const struct device *const ipm_handle = DEVICE_DT_GET(DT_CHOSEN(zephyr_ipc));

/* metal_phys_addr_t is just a physical-memory-address-sized integer type.
 * shm_physmap is the physical address of the shared-memory region both
 * cores use to exchange RPMsg message data; rsc_tab_physmap is the address
 * of the "resource table" -- a small structure, generated at build time,
 * that tells Linux's remoteproc driver what memory regions and mailbox this
 * firmware expects to use. */
static metal_phys_addr_t shm_physmap;
static metal_phys_addr_t rsc_tab_physmap;

/* "metal_io_region" is libmetal's abstraction for a chunk of memory that can
 * be safely read/written by both cores -- it wraps the raw physical address
 * with the bookkeeping OpenAMP's internals need. One region for the shared
 * message buffers, one for the resource table itself. */
static struct metal_io_region shm_io_data;
static struct metal_io_region rsc_io_data;
static struct metal_io_region *shm_io = &shm_io_data;
static struct metal_io_region *rsc_io = &rsc_io_data;

/* The "virtio device" is OpenAMP's model of this RPMsg link as if it were a
 * virtio device (the same abstraction Linux uses for other paravirtualized
 * devices) -- rvdev is the concrete object representing it. rsc_table is a
 * pointer to the resource table mentioned above (its contents are generated
 * by Zephyr's build system, not written by hand here). rpdev is the actual
 * "rpmsg device" handle other code uses to create endpoints and send/receive
 * messages once the link is up. */
static struct rpmsg_virtio_device rvdev;
static void *rsc_table;
static struct rpmsg_device *rpdev;

/* The one RPMsg endpoint this firmware exposes, and the buffer its receive
 * callback copies incoming message bytes into. `raw_rx_len` is `volatile`
 * because it's written by the receive callback (which Zephyr may run on a
 * different thread's context than the one reading it) and read by
 * rpmsg_raw_thread_entry -- `volatile` tells the compiler not to cache this
 * value in a register across that handoff. */
static struct rpmsg_endpoint raw_ept;
static char raw_rx_buf[RPMSG_RX_BUF_SIZE];
static volatile size_t raw_rx_len;

/* Three semaphores, each used as a simple one-way signal between threads
 * (K_SEM_DEFINE(name, initial_count, max_count) -- these all start at 0,
 * meaning "not yet signaled", and can only ever be signaled once at a time
 * since max_count is 1):
 *   - mbox_notify_sem: "a mailbox interrupt arrived" (IPM callback -> rpmsg_mng_task)
 *   - raw_ept_ready_sem: "the rpmsg device is ready, go create your endpoint"
 *     (rpmsg_mng_task -> rpmsg_raw_thread_entry, once, at startup)
 *   - raw_rx_sem: "a full message arrived on our endpoint" (receive callback
 *     -> rpmsg_raw_thread_entry, once per incoming message)
 */
static K_SEM_DEFINE(mbox_notify_sem, 0, 1);
static K_SEM_DEFINE(raw_ept_ready_sem, 0, 1);
static K_SEM_DEFINE(raw_rx_sem, 0, 1);

/* Called by Zephyr's IPM driver whenever the A7 core signals the mailbox --
 * i.e. "something changed in the shared virtqueues, go take a look." This
 * runs in interrupt-adjacent context, so it does the absolute minimum: wake
 * up rpmsg_mng_task, which does the real work outside of interrupt context. */
static void platform_ipm_callback(const struct device *dev, void *context,
                                   uint32_t id, volatile void *data) {
  ARG_UNUSED(dev);
  ARG_UNUSED(context);
  ARG_UNUSED(id);
  ARG_UNUSED(data);

  k_sem_give(&mbox_notify_sem);
}

/* Called by OpenAMP itself whenever a full message arrives on our "rpmsg-raw"
 * endpoint. This is still "transport" code, not "LED" code -- it just copies
 * the bytes out of OpenAMP's internal buffer (which it can't hold onto past
 * this callback) into our own buffer, then wakes up rpmsg_raw_thread_entry
 * to actually interpret them. */
static int rpmsg_recv_raw_callback(struct rpmsg_endpoint *ept, void *data,
                                    size_t len, uint32_t src, void *priv) {
  ARG_UNUSED(ept);
  ARG_UNUSED(src);
  ARG_UNUSED(priv);

  /* Truncate rather than overflow if something sends an oversized message --
   * commands here are always tiny, so this should never actually trigger. */
  if (len >= sizeof(raw_rx_buf)) {
    len = sizeof(raw_rx_buf) - 1;
  }
  memcpy(raw_rx_buf, data, len);
  raw_rx_len = len;
  k_sem_give(&raw_rx_sem);

  return RPMSG_SUCCESS;
}

/* OpenAMP calls this if the *other* side (Linux) ever tries to announce a
 * new named service we didn't ask for. We only ever create the one
 * "rpmsg-raw" endpoint ourselves, so this should never fire in normal
 * operation -- it's here so an unexpected announcement gets logged instead
 * of silently ignored. */
static void new_service_cb(struct rpmsg_device *rdev, const char *name, uint32_t src) {
  ARG_UNUSED(rdev);
  ARG_UNUSED(src);

  printk("rpmsg: unexpected ns service receive for name %s\n", name);
}

/* Called by OpenAMP whenever it needs to tell the A7 "I've put something in
 * the shared memory, go look" -- the actual mechanism for that is this
 * function ringing the IPCC hardware mailbox via ipm_send().
 *
 * The #if/#else here encodes a real hardware fact, not a style choice: this
 * board's IPCC mailbox is a pure doorbell -- it can signal "something
 * happened, here's an ID," but it cannot carry any data payload
 * (CONFIG_IPM_MAX_DATA_SIZE is 0 for this board's IPM driver). Calling
 * ipm_send() with a non-empty payload on hardware like that doesn't fail
 * loudly -- it gets silently rejected by the driver, so the notification
 * never actually reaches Linux even though this function returns success.
 * This exact mistake was made once already while building this feature (see
 * the GitHub wiki's "M4-Firmware" page for the full debugging story) --
 * three full rebuild-and-reflash cycles to track down, because every symptom
 * on the M4 side looked like success. The #if below is what a *correct*
 * zero-payload send looks like for this hardware. */
static int mailbox_notify(void *priv, uint32_t id) {
  ARG_UNUSED(priv);

#if CONFIG_IPM_MAX_DATA_SIZE > 0
  ipm_send(ipm_handle, 0, id, &id, 4);
#else
  ipm_send(ipm_handle, 0, id, NULL, 0);
#endif

  return 0;
}

/* One-time setup: initializes libmetal, describes the shared-memory and
 * resource-table regions to it, and enables the IPM mailbox so
 * platform_ipm_callback (above) actually gets called when the A7 signals us.
 * Returns 0 on success, -1 on failure (each failure path prints exactly what
 * went wrong before returning, since a silent failure here would leave the
 * whole RPMsg link permanently dead with no clue why). */
static int platform_init(void) {
  struct metal_init_params metal_params = METAL_INIT_DEFAULTS;
  int rsc_size;
  int status;

  status = metal_init(&metal_params);
  if (status) {
    printk("metal_init failed: %d\n", status);
    return -1;
  }

  /* DT_CHOSEN(zephyr_ipc_shm) is the devicetree node the board overlay
   * designated as the RPMsg shared-memory carveout (`chosen { zephyr,ipc_shm
   * = &mcusram3; }`). DT_REG_ADDR/DT_REG_SIZE read that node's address and
   * size straight out of the devicetree at compile time. */
  shm_physmap = DT_REG_ADDR(DT_CHOSEN(zephyr_ipc_shm));
  metal_io_init(shm_io, (void *)shm_physmap, &shm_physmap,
                DT_REG_SIZE(DT_CHOSEN(zephyr_ipc_shm)), -1, 0,
                addr_translation_get_ops(shm_physmap));

  /* rsc_table_get() is generated code (from Zephyr's subsys/ipc/open-amp/
   * resource_table.c) that hands back the address and size of the
   * build-time-generated resource table described near the top of this
   * file. */
  rsc_table_get(&rsc_table, &rsc_size);
  rsc_tab_physmap = (uintptr_t)rsc_table;
  metal_io_init(rsc_io, rsc_table, &rsc_tab_physmap, rsc_size, -1, 0, NULL);

  if (!device_is_ready(ipm_handle)) {
    printk("IPM device is not ready\n");
    return -1;
  }

  ipm_register_callback(ipm_handle, platform_ipm_callback, NULL);

  status = ipm_set_enabled(ipm_handle, 1);
  if (status) {
    printk("ipm_set_enabled failed: %d\n", status);
    return -1;
  }

  return 0;
}

/* Creates the OpenAMP "virtio device" that represents this RPMsg link, sets
 * up its two virtqueues (one for each direction), and returns the resulting
 * `rpmsg_device` handle that the rest of this file uses to actually send and
 * receive messages. This is dense, low-level OpenAMP API usage copied
 * near-verbatim from Zephyr's openamp_rsc_table sample (see the file header
 * comment) -- the short version of what it does: read the vring addresses
 * and sizes the resource table describes, hand them to OpenAMP's
 * vring-management functions, and wire mailbox_notify() in as "how to tell
 * the other side something changed." Returns NULL on any failure, after
 * printing which step failed. */
static struct rpmsg_device *platform_create_rpmsg_vdev(rpmsg_ns_bind_cb ns_cb) {
  struct fw_rsc_vdev_vring *vring_rsc;
  struct virtio_device *vdev;
  int ret;

  /* VIRTIO_DEV_DEVICE: we are the "device" side of this virtio link (Linux
   * is the "driver"/host side) -- this is fixed by the AMP topology (the A7
   * creates and owns the vrings), not a preference. */
  vdev = rproc_virtio_create_vdev(VIRTIO_DEV_DEVICE, VDEV_ID,
                                   rsc_table_to_vdev(rsc_table),
                                   rsc_io, NULL, mailbox_notify, NULL);
  if (!vdev) {
    printk("failed to create vdev\n");
    return NULL;
  }

  /* Blocks until the A7 side has finished its own setup and is ready to
   * exchange messages -- see rproc_virtio_wait_remote_ready()'s
   * implementation: it just polls a status flag in the resource table that
   * Linux sets once its virtio_rpmsg_bus driver has attached. */
  rproc_virtio_wait_remote_ready(vdev);

  /* Vring 0 and vring 1 are the two directions of the shared-memory message
   * queue (one M4->A7, one A7->M4). Their addresses/sizes come from the
   * resource table, generated at build time from the board overlay's shared
   * memory reservation. */
  vring_rsc = rsc_table_get_vring0(rsc_table);
  ret = rproc_virtio_init_vring(vdev, 0, vring_rsc->notifyid,
                                 (void *)vring_rsc->da, rsc_io,
                                 vring_rsc->num, vring_rsc->align);
  if (ret) {
    printk("failed to init vring 0: %d\n", ret);
    goto failed;
  }

  vring_rsc = rsc_table_get_vring1(rsc_table);
  ret = rproc_virtio_init_vring(vdev, 1, vring_rsc->notifyid,
                                 (void *)vring_rsc->da, rsc_io,
                                 vring_rsc->num, vring_rsc->align);
  if (ret) {
    printk("failed to init vring 1: %d\n", ret);
    goto failed;
  }

  /* rpmsg_init_vdev wires the vdev + vrings into the higher-level RPMsg
   * protocol layer (endpoint creation, message framing) -- everything
   * *before* this line is "raw virtio," everything after is "RPMsg." */
  ret = rpmsg_init_vdev(&rvdev, vdev, ns_cb, shm_io, NULL);
  if (ret) {
    printk("failed rpmsg_init_vdev: %d\n", ret);
    goto failed;
  }

  return rpmsg_virtio_get_rpmsg_device(&rvdev);

failed:
  rproc_virtio_remove_vdev(vdev);
  return NULL;
}

/* This thread does the one-time OpenAMP setup above, then spends the rest of
 * its life in a tight "wait for a mailbox interrupt, tell OpenAMP to process
 * whatever arrived" loop. It never looks at message *contents* -- that's
 * rpmsg_raw_thread_entry's job, below. */
static void rpmsg_mng_task(void *arg1, void *arg2, void *arg3) {
  ARG_UNUSED(arg1);
  ARG_UNUSED(arg2);
  ARG_UNUSED(arg3);

  if (platform_init()) {
    printk("rpmsg: platform_init failed, IPC disabled\n");
    return;
  }

  rpdev = platform_create_rpmsg_vdev(new_service_cb);
  if (!rpdev) {
    printk("rpmsg: failed to create rpmsg virtio device, IPC disabled\n");
    return;
  }

  /* rpdev is ready -- let the endpoint-owning thread know it can create its
   * endpoint now. This only ever fires once, at startup. */
  k_sem_give(&raw_ept_ready_sem);

  /* rproc_virtio_notified() is what actually walks the virtqueues and fires
   * receive callbacks (like rpmsg_recv_raw_callback) for anything new that
   * arrived -- it does nothing on its own until something wakes this loop up
   * via mbox_notify_sem (given by platform_ipm_callback above). */
  while (1) {
    k_sem_take(&mbox_notify_sem, K_FOREVER);
    rproc_virtio_notified(rvdev.vdev, VRING1_ID);
  }
}

/* ===========================================================================
 * Section: LED command handling
 * ===========================================================================
 * This is the actual "feature" of this firmware. Everything above this line
 * exists purely to make the two lines below ("a message arrived" /
 * "send a reply") possible.
 *
 * Protocol (deliberately tiny, plain-text, one command per message -- there's
 * no reason for anything more elaborate at this stage, and RPMsg already
 * frames each message as one discrete unit, so there's no need for a
 * delimiter the way there would be over a raw byte stream like TCP):
 *
 *   Request from the A7           Reply from the M4
 *   ----------------------------  ---------------------------------
 *   "LED ON"                      "LED ON"   (LED is now on)
 *   "LED OFF"                     "LED OFF"  (LED is now off)
 *   "LED STATUS"                  "LED ON" or "LED OFF" (unchanged)
 *   anything else                 "ERR: unknown command"
 *
 * The reply always states the LED's *resulting* state (even for "LED
 * STATUS", which doesn't change it) -- that way the A7 side never has to
 * separately track "did my command actually take effect," it just believes
 * whatever the M4 last said.
 */

/* Turns the LED on or off, and updates our own record of its state. This is
 * the only place in this file that ever touches the LED's GPIO -- everything
 * else in this section just decides *when* to call this. */
static void set_led(bool on) {
  /* gpio_pin_set_dt() takes a logical 0/1 value, not a raw electrical level
   * -- the `GPIO_ACTIVE_HIGH`/`GPIO_ACTIVE_LOW` flag baked into the `led`
   * devicetree spec (see its definition near the top of this file) is what
   * translates "1 means on" into the actual voltage the hardware needs. */
  gpio_pin_set_dt(&led, on ? 1 : 0);
  led_is_on = on;
}

/* Parses one incoming command and returns the reply text to send back.
 * `out` is a caller-provided buffer (so this function doesn't need to
 * allocate anything -- there's no heap-free-standing allocator expected to
 * exist on this side, see ARCHITECTURE.md's M4 section for why); returns the
 * number of bytes written into `out`, for the caller to pass straight to
 * rpmsg_send(). */
static int handle_led_command(const char *cmd, size_t cmd_len, char *out, size_t out_size) {
  if (cmd_len == strlen("LED ON") && memcmp(cmd, "LED ON", cmd_len) == 0) {
    set_led(true);
  } else if (cmd_len == strlen("LED OFF") && memcmp(cmd, "LED OFF", cmd_len) == 0) {
    set_led(false);
  } else if (cmd_len == strlen("LED STATUS") && memcmp(cmd, "LED STATUS", cmd_len) == 0) {
    /* Deliberately does nothing to the LED -- just falls through to the
     * reply below, which always reports the current state regardless of
     * which of the three recognized commands triggered it. */
  } else {
    return snprintf(out, out_size, "ERR: unknown command");
  }

  return snprintf(out, out_size, led_is_on ? "LED ON" : "LED OFF");
}

/* Owns the "rpmsg-raw" endpoint: creates it once (after waiting for
 * rpmsg_mng_task to signal the transport is ready), then loops forever
 * waiting for a message to arrive (raw_rx_sem, given by
 * rpmsg_recv_raw_callback above), handing it to handle_led_command(), and
 * sending the resulting reply back over the same endpoint. */
static void rpmsg_raw_thread_entry(void *arg1, void *arg2, void *arg3) {
  ARG_UNUSED(arg1);
  ARG_UNUSED(arg2);
  ARG_UNUSED(arg3);

  char reply_buf[64];
  int ret;

  k_sem_take(&raw_ept_ready_sem, K_FOREVER);

  ret = rpmsg_create_ept(&raw_ept, rpdev, RPMSG_RAW_CHANNEL_NAME,
                          RPMSG_ADDR_ANY, RPMSG_ADDR_ANY,
                          rpmsg_recv_raw_callback, NULL);
  if (ret) {
    printk("rpmsg-raw: could not create endpoint: %d\n", ret);
    return;
  }

  printk("rpmsg-raw: endpoint ready, LED starts OFF\n");

  while (1) {
    k_sem_take(&raw_rx_sem, K_FOREVER);

    int reply_len = handle_led_command((const char *)raw_rx_buf, raw_rx_len,
                                        reply_buf, sizeof(reply_buf));
    rpmsg_send(&raw_ept, reply_buf, reply_len);
  }
}

/* ===========================================================================
 * Entry point
 * ===========================================================================
 * Zephyr calls this automatically at boot, on what's effectively the "main"
 * thread. All it does is start the two threads described above and return
 * -- there's no reason for this thread to do anything else once that's done,
 * unlike issue #11's version of this file, which ran a heartbeat loop
 * directly here (removed now, see this file's header comment). */
int main(void) {
  printk("Starting application threads!\n");

  /* Make sure the LED starts in a known, deterministic state (off) rather
   * than whatever the GPIO happened to reset to in hardware -- set_led()
   * both drives the pin and updates led_is_on, so this also makes the very
   * first "LED STATUS" reply correct even before any ON/OFF command has ever
   * been sent. */
  if (gpio_is_ready_dt(&led)) {
    gpio_pin_configure_dt(&led, GPIO_OUTPUT_INACTIVE);
    set_led(false);
  } else {
    printk("LED GPIO device not ready\n");
  }

  k_thread_create(&rpmsg_mng_thread_data, rpmsg_mng_stack,
                   K_THREAD_STACK_SIZEOF(rpmsg_mng_stack), rpmsg_mng_task,
                   NULL, NULL, NULL, K_PRIO_COOP(8), 0, K_NO_WAIT);
  k_thread_create(&rpmsg_raw_thread_data, rpmsg_raw_stack,
                   K_THREAD_STACK_SIZEOF(rpmsg_raw_stack), rpmsg_raw_thread_entry,
                   NULL, NULL, NULL, K_PRIO_COOP(7), 0, K_NO_WAIT);

  return 0;
}
