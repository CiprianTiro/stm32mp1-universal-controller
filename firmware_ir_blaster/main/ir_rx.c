/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * IR receive -- see ir_rx.h.
 *
 * How the pieces fit:
 *
 *   1. rmt_receive() arms the hardware: "record the next frame into
 *      rx_symbols[]". It returns immediately.
 *   2. When the pin has been quiet for IR_RX_END_GAP_NS, the hardware
 *      decides the frame is over and the driver calls rx_done_isr() --
 *      in interrupt context, where we may only do very little.
 *   3. rx_done_isr() just posts "a frame is ready" to a queue.
 *   4. rx_task() (a normal FreeRTOS task) wakes up on that queue, copies
 *      the frame, calls the handler, and goes back to step 1.
 */

#include "ir_rx.h"

#include "driver/rmt_rx.h"
#include "esp_check.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"

static const char *TAG = "ir_rx";

/* Pulses shorter than this are noise, not IR, and are ignored. The
 * hardware filter can't go above ~3.2 us on the ESP32-S3; real IR marks
 * and spaces are at least ~250 us, so 1.25 us is safely in between. */
#define IR_RX_GLITCH_NS 1250

/* If the pin stays unchanged this long, the frame is over. Must be longer
 * than any space *inside* a frame (NEC's longest is 4.5 ms; some AC
 * protocols pause up to ~15 ms between parts) and at most 32.7 ms, the
 * longest duration the RMT hardware can count at 1 MHz. */
#define IR_RX_END_GAP_NS (20 * 1000 * 1000)

static rmt_channel_handle_t rx_channel;
static QueueHandle_t rx_queue;
static ir_rx_handler_t rx_handler;

/* The hardware writes received symbols here. Static, so it's never on a
 * task stack and always valid while the hardware is using it. */
static rmt_symbol_word_t rx_symbols[IR_FRAME_MAX_SYMBOLS];

/* Copy handed to the handler, so the next receive can already start. */
static ir_frame_t rx_frame;

static const rmt_receive_config_t receive_config = {
  .signal_range_min_ns = IR_RX_GLITCH_NS,
  .signal_range_max_ns = IR_RX_END_GAP_NS,
};

/* Interrupt context: only tell the task, nothing else. The return value
 * tells FreeRTOS whether a higher-priority task was woken (so it can switch
 * to it right away). */
static bool rx_done_isr(rmt_channel_handle_t channel, const rmt_rx_done_event_data_t *edata,
                        void *user_ctx)
{
  BaseType_t woken = pdFALSE;
  xQueueSendFromISR(rx_queue, edata, &woken);
  return woken == pdTRUE;
}

/* Pulses shorter than this are noise even though they got past the
 * hardware filter above. Real IR protocols never go below ~250 us (NEC 560,
 * Sharp 320, RC5 889), but receivers on a breadboard show blips of
 * ~100-200 us -- e.g. a 9000 us NEC leader cut in two by a 158 us "gap".
 * Such blips are joined back into what surrounds them. */
#define IR_RX_MIN_PULSE_US 200

/* A real frame has at least 2 marks (an NEC repeat: leader + stop mark).
 * Anything shorter left after filtering is noise and not handed over. */
#define IR_RX_MIN_SYMBOLS 2

/* Working space for normalize(): the received levels, one entry per half
 * symbol. Static because it's ~5 KB, too big for the task's stack. */
static uint8_t half_level[IR_FRAME_MAX_SYMBOLS * 2];
static uint32_t half_us[IR_FRAME_MAX_SYMBOLS * 2];

/* Copies the received symbols into `out`, cleaned up so that every symbol is
 * exactly "mark, then space" (ir_frame.h), the way the decoders and the
 * transmitter expect it. Three steps:
 *
 *   1. Unpack. Each RMT symbol holds two (level, duration) halves, recorded
 *      in the order they happened. Walk through them one by one; a zero
 *      duration means "end of data". Spaces before the first mark are just
 *      idle and are skipped -- otherwise every symbol would come out as
 *      "space, mark", shifted by one half.
 *
 *   2. Remove blips. A half shorter than IR_RX_MIN_PULSE_US is added to the
 *      half before it; the half after it then has the same level as that
 *      one and gets added too. So mark 2341 + blip 158 + mark 5919 becomes
 *      one 8418 us mark, as the remote actually sent. A short mark at the
 *      very start (nothing before it) is dropped.
 *
 *   3. Pack the halves back into symbols: (mark, space) pairs. The last mark
 *      has no space after it; a space of 0 also tells the transmitter
 *      "end of frame". */
static void normalize(const rmt_symbol_word_t *in, size_t in_count, ir_frame_t *out)
{
  size_t halves = 0;

  /* Steps 1 + 2 together: each new half is either dropped, joined into the
   * previous one, or appended. */
  for (size_t i = 0; i < in_count * 2 && halves < IR_FRAME_MAX_SYMBOLS * 2; i++) {
    const rmt_symbol_word_t *s = &in[i / 2];
    uint8_t level = (i % 2 == 0) ? s->level0 : s->level1;
    uint32_t duration = (i % 2 == 0) ? s->duration0 : s->duration1;

    if (duration == 0) {
      break; /* end of data */
    }
    if (halves == 0) {
      /* Nothing kept yet: skip idle spaces and a leading blip. */
      if (level == 1 && duration >= IR_RX_MIN_PULSE_US) {
        half_level[0] = 1;
        half_us[0] = duration;
        halves = 1;
      }
      continue;
    }
    if (duration < IR_RX_MIN_PULSE_US || level == half_level[halves - 1]) {
      /* A blip, or the continuation after one: extend the previous half. */
      half_us[halves - 1] += duration;
      continue;
    }
    half_level[halves] = level;
    half_us[halves] = duration;
    halves++;
  }

  /* Step 3: pack. halves[] alternates mark, space, mark, ... from index 0.
   * A duration can't exceed what the RMT field holds (32.7 ms). */
  size_t n = 0;
  for (size_t h = 0; h < halves && n < IR_FRAME_MAX_SYMBOLS; h += 2) {
    uint32_t mark = half_us[h];
    uint32_t space = (h + 1 < halves) ? half_us[h + 1] : 0;
    out->symbols[n++] = ir_symbol(mark > IR_MAX_DURATION_US ? IR_MAX_DURATION_US : mark,
                                  space > IR_MAX_DURATION_US ? IR_MAX_DURATION_US : space);
  }
  out->count = n;
}

static void rx_task(void *arg)
{
  rmt_rx_done_event_data_t event;

  for (;;) {
    ESP_ERROR_CHECK(rmt_receive(rx_channel, rx_symbols, sizeof(rx_symbols), &receive_config));

    /* Sleep until a frame arrives -- no polling, no CPU used meanwhile. */
    xQueueReceive(rx_queue, &event, portMAX_DELAY);

    normalize(event.received_symbols, event.num_symbols, &rx_frame);

    if (rx_frame.count >= IR_RX_MIN_SYMBOLS && rx_handler) {
      rx_handler(&rx_frame);
    }
  }
}

esp_err_t ir_rx_start(gpio_num_t gpio, ir_rx_handler_t handler)
{
  rx_handler = handler;

  rmt_rx_channel_config_t config = {
    .gpio_num = gpio,
    .clk_src = RMT_CLK_SRC_DEFAULT,
    .resolution_hz = IR_RESOLUTION_HZ,
    /* Hardware buffer: 2 blocks of 48 symbols; longer frames are copied
     * out to rx_symbols[] while they arrive ("ping-pong"). */
    .mem_block_symbols = 96,
    /* The receiver's OUT is LOW during a mark. Inverting makes a mark
     * level 1, the same convention the transmitter uses (ir_frame.h), so a
     * captured frame can be sent back out unchanged. */
    .flags.invert_in = 1,
  };
  ESP_RETURN_ON_ERROR(rmt_new_rx_channel(&config, &rx_channel), TAG, "create RX channel");

  rx_queue = xQueueCreate(1, sizeof(rmt_rx_done_event_data_t));
  ESP_RETURN_ON_FALSE(rx_queue, ESP_ERR_NO_MEM, TAG, "create queue");

  rmt_rx_event_callbacks_t callbacks = { .on_recv_done = rx_done_isr };
  ESP_RETURN_ON_ERROR(rmt_rx_register_event_callbacks(rx_channel, &callbacks, NULL), TAG,
                      "register callback");
  ESP_RETURN_ON_ERROR(rmt_enable(rx_channel), TAG, "enable RX channel");

  /* 4 KB stack: the handler prints, and printf needs some room. */
  BaseType_t ok = xTaskCreate(rx_task, "ir_rx", 4096, NULL, 5, NULL);
  ESP_RETURN_ON_FALSE(ok == pdPASS, ESP_ERR_NO_MEM, TAG, "create task");

  ESP_LOGI(TAG, "IR receiver listening on GPIO%d", gpio);
  return ESP_OK;
}
