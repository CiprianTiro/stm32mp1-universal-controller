/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 */

#include <zephyr/kernel.h>
#include <zephyr/device.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/drivers/ipm.h>
#include <zephyr/sys/printk.h>
#include <stdio.h>
#include <string.h>

#include <openamp/open_amp.h>
#include <metal/sys.h>
#include <metal/io.h>
#include <resource_table.h>
#include <addr_translation.h>

/*
 * RPMsg link to backend_daemon (issue #12). This borrows its OpenAMP
 * plumbing (platform_init / platform_create_rpmsg_vdev / the IPM-driven
 * receive loop below) near-verbatim from Zephyr's own
 * samples/subsys/ipc/openamp_rsc_table sample - the only sample with board
 * files proven for stm32mp157c_dk2. What's project-specific is trimmed to
 * one endpoint: "rpmsg-raw", the exact channel name Linux's in-tree
 * rpmsg_char driver auto-binds to (drivers/rpmsg/rpmsg_char.c's
 * rpmsg_chrdev_id_table) - any other name would need its own kernel
 * driver, same as ST's "rpmsg-client-sample"/"rpmsg-tty" samples do.
 *
 * Protocol (v0, deliberately minimal - not meant to survive Sprint 3):
 * backend_daemon sends an arbitrary text request; this replies with
 * "ACK <n>: <original request>", where <n> is a message counter kept on
 * this side. Proves a genuine two-way round trip (the counter is M4-side
 * state, not just a loopback) without needing a real framing format yet.
 */

#define HEARTBEAT_PERIOD_MS 1000
#define RPMSG_RAW_CHANNEL_NAME "rpmsg-raw"
#define RPMSG_RX_BUF_SIZE 256

static const struct gpio_dt_spec led = GPIO_DT_SPEC_GET(DT_ALIAS(led0), gpios);

K_THREAD_STACK_DEFINE(rpmsg_mng_stack, 1024);
K_THREAD_STACK_DEFINE(rpmsg_raw_stack, 1024);
static struct k_thread rpmsg_mng_thread_data;
static struct k_thread rpmsg_raw_thread_data;

static const struct device *const ipm_handle = DEVICE_DT_GET(DT_CHOSEN(zephyr_ipc));

static metal_phys_addr_t shm_physmap;
static metal_phys_addr_t rsc_tab_physmap;
static struct metal_io_region shm_io_data;
static struct metal_io_region rsc_io_data;
static struct metal_io_region *shm_io = &shm_io_data;
static struct metal_io_region *rsc_io = &rsc_io_data;
static struct rpmsg_virtio_device rvdev;
static void *rsc_table;
static struct rpmsg_device *rpdev;

static struct rpmsg_endpoint raw_ept;
static char raw_rx_buf[RPMSG_RX_BUF_SIZE];
static volatile size_t raw_rx_len;

static K_SEM_DEFINE(mbox_notify_sem, 0, 1);
static K_SEM_DEFINE(raw_ept_ready_sem, 0, 1);
static K_SEM_DEFINE(raw_rx_sem, 0, 1);

/* Runs directly on the main thread (called from main() below, not spawned)
 * - main() never needs to do anything else once the other two threads are
 * started, so there's no reason to burn a third stack just to also make
 * this one an explicit thread. */
static void run_heartbeat_forever(void)
{
	if (!gpio_is_ready_dt(&led)) {
		printk("Heartbeat LED device not ready\n");
		return;
	}

	gpio_pin_configure_dt(&led, GPIO_OUTPUT_INACTIVE);

	uint32_t beat = 0;

	while (1) {
		gpio_pin_toggle_dt(&led);
		printk("M4 heartbeat: %u\n", beat++);
		k_msleep(HEARTBEAT_PERIOD_MS);
	}
}

static void platform_ipm_callback(const struct device *dev, void *context,
				   uint32_t id, volatile void *data)
{
	ARG_UNUSED(dev);
	ARG_UNUSED(context);
	ARG_UNUSED(id);
	ARG_UNUSED(data);

	k_sem_give(&mbox_notify_sem);
}

static int rpmsg_recv_raw_callback(struct rpmsg_endpoint *ept, void *data,
				    size_t len, uint32_t src, void *priv)
{
	ARG_UNUSED(ept);
	ARG_UNUSED(src);
	ARG_UNUSED(priv);

	if (len >= sizeof(raw_rx_buf)) {
		len = sizeof(raw_rx_buf) - 1;
	}
	memcpy(raw_rx_buf, data, len);
	raw_rx_len = len;
	k_sem_give(&raw_rx_sem);

	return RPMSG_SUCCESS;
}

static void new_service_cb(struct rpmsg_device *rdev, const char *name, uint32_t src)
{
	ARG_UNUSED(rdev);
	ARG_UNUSED(src);

	printk("rpmsg: unexpected ns service receive for name %s\n", name);
}

static int mailbox_notify(void *priv, uint32_t id)
{
	ARG_UNUSED(priv);

	/* STM32's IPCC mailbox is a pure doorbell (CONFIG_IPM_MAX_DATA_SIZE=0
	 * on this board) - it can signal an id, but carries no payload. A
	 * non-empty send here is silently rejected by the IPM driver, so the
	 * kick never reaches Linux even though this call still returns
	 * success: the NS announcement (or any later message) sits in the
	 * vring forever, acked on the M4 side, never actually delivered. */
#if CONFIG_IPM_MAX_DATA_SIZE > 0
	ipm_send(ipm_handle, 0, id, &id, 4);
#else
	ipm_send(ipm_handle, 0, id, NULL, 0);
#endif

	return 0;
}

static int platform_init(void)
{
	struct metal_init_params metal_params = METAL_INIT_DEFAULTS;
	int rsc_size;
	int status;

	status = metal_init(&metal_params);
	if (status) {
		printk("metal_init failed: %d\n", status);
		return -1;
	}

	shm_physmap = DT_REG_ADDR(DT_CHOSEN(zephyr_ipc_shm));
	metal_io_init(shm_io, (void *)shm_physmap, &shm_physmap,
		      DT_REG_SIZE(DT_CHOSEN(zephyr_ipc_shm)), -1, 0,
		      addr_translation_get_ops(shm_physmap));

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

static struct rpmsg_device *platform_create_rpmsg_vdev(rpmsg_ns_bind_cb ns_cb)
{
	struct fw_rsc_vdev_vring *vring_rsc;
	struct virtio_device *vdev;
	int ret;

	vdev = rproc_virtio_create_vdev(VIRTIO_DEV_DEVICE, VDEV_ID,
					 rsc_table_to_vdev(rsc_table),
					 rsc_io, NULL, mailbox_notify, NULL);
	if (!vdev) {
		printk("failed to create vdev\n");
		return NULL;
	}

	rproc_virtio_wait_remote_ready(vdev);

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

/* Owns the endpoint and replies to each request - kept separate from
 * rpmsg_mng_task below so a slow/blocked responder can never stall the
 * mailbox-notify pump those other messages (there are none yet, but the
 * next endpoint added in Sprint 3 would share that pump). */
static void rpmsg_raw_thread_entry(void *arg1, void *arg2, void *arg3)
{
	ARG_UNUSED(arg1);
	ARG_UNUSED(arg2);
	ARG_UNUSED(arg3);

	char tx_buf[RPMSG_RX_BUF_SIZE + 16];
	uint32_t msg_count = 0;
	int ret;

	k_sem_take(&raw_ept_ready_sem, K_FOREVER);

	ret = rpmsg_create_ept(&raw_ept, rpdev, RPMSG_RAW_CHANNEL_NAME,
				RPMSG_ADDR_ANY, RPMSG_ADDR_ANY,
				rpmsg_recv_raw_callback, NULL);

	if (ret) {
		printk("rpmsg-raw: could not create endpoint: %d\n", ret);
		return;
	}

	printk("rpmsg-raw: endpoint ready\n");

	while (1) {
		k_sem_take(&raw_rx_sem, K_FOREVER);

		msg_count++;
		int n = snprintf(tx_buf, sizeof(tx_buf), "ACK %u: %.*s",
				  msg_count, (int)raw_rx_len, raw_rx_buf);
		rpmsg_send(&raw_ept, tx_buf, n);
	}
}

static void rpmsg_mng_task(void *arg1, void *arg2, void *arg3)
{
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

	k_sem_give(&raw_ept_ready_sem);

	while (1) {
		k_sem_take(&mbox_notify_sem, K_FOREVER);
		rproc_virtio_notified(rvdev.vdev, VRING1_ID);
	}
}

int main(void)
{
	printk("Starting application threads!\n");

	k_thread_create(&rpmsg_mng_thread_data, rpmsg_mng_stack,
			 K_THREAD_STACK_SIZEOF(rpmsg_mng_stack), rpmsg_mng_task,
			 NULL, NULL, NULL, K_PRIO_COOP(8), 0, K_NO_WAIT);
	k_thread_create(&rpmsg_raw_thread_data, rpmsg_raw_stack,
			 K_THREAD_STACK_SIZEOF(rpmsg_raw_stack), rpmsg_raw_thread_entry,
			 NULL, NULL, NULL, K_PRIO_COOP(7), 0, K_NO_WAIT);

	run_heartbeat_forever();

	return 0;
}
