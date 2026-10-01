/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * IR service -- see ir_service.h.
 */

#include "ir_service.h"

#include "esp_check.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "ir_nec.h"
#include "ir_rx.h"
#include "ir_tx.h"

static const char *TAG = "ir_service";

/* A frame counts as our own echo if it arrives while we send or within
 * this long after: the receiver hands a frame over only after 20 ms of
 * silence (ir_rx.c), so an echo arrives a bit after the send ends. */
#define ECHO_WINDOW_US (150 * 1000)

/* Unrecognised frames shorter than this are noise, not a remote: real IR
 * protocols send at least ~10 marks (Sony 13, RC5 10-14, NEC 34); short
 * ones like NEC's repeat are recognised by their decoder first. */
#define MIN_UNKNOWN_MARKS 8

/* NEC frames start every 108 ms while a button is held. */
#define NEC_FRAME_PERIOD_MS 108

#define MAX_LISTENERS 4

static ir_listener_t listeners[MAX_LISTENERS];
static int listener_count;

/* Only one send at a time: the console and the hub may both ask. */
static SemaphoreHandle_t send_lock;
static volatile bool sending;
static volatile int64_t send_end_us;

/* Frame being sent. Only used while send_lock is held. Static: 2 KB. */
static ir_frame_t tx_frame;

/* The last real frame heard, for `replay`. Written by the receive task,
 * read by others -- guarded by its own lock. */
static ir_frame_t last_frame;
static SemaphoreHandle_t last_lock;

/* Receive task: every frame heard (ir_rx.c). */
static void on_frame(const ir_frame_t *frame)
{
  ir_nec_result_t nec;
  ir_nec_kind_t kind = ir_nec_decode(frame, &nec);
  if (kind == IR_NEC_NONE && frame->count < MIN_UNKNOWN_MARKS) {
    return; /* noise */
  }

  bool echo = sending || esp_timer_get_time() - send_end_us < ECHO_WINDOW_US;

  if (!echo) {
    /* Keep it for replay -- but not an NEC "button held" repeat: replaying
     * that alone does nothing, the frame before it is what counts. */
    if (kind != IR_NEC_REPEAT) {
      xSemaphoreTake(last_lock, portMAX_DELAY);
      last_frame = *frame;
      xSemaphoreGive(last_lock);
    }
  }

  for (int i = 0; i < listener_count; i++) {
    listeners[i](frame, echo);
  }
}

esp_err_t ir_service_start(gpio_num_t tx_gpio, gpio_num_t rx_gpio)
{
  send_lock = xSemaphoreCreateMutex();
  last_lock = xSemaphoreCreateMutex();
  ESP_RETURN_ON_FALSE(send_lock && last_lock, ESP_ERR_NO_MEM, TAG, "locks");

  ESP_RETURN_ON_ERROR(ir_tx_init(tx_gpio), TAG, "transmitter");
  return ir_rx_start(rx_gpio, on_frame);
}

void ir_service_add_listener(ir_listener_t listener)
{
  if (listener_count < MAX_LISTENERS) {
    listeners[listener_count++] = listener;
  }
}

/* Sends tx_frame. Caller holds send_lock. */
static esp_err_t send_locked(void)
{
  sending = true;
  esp_err_t err = ir_tx_send(&tx_frame);
  send_end_us = esp_timer_get_time();
  sending = false;
  return err;
}

esp_err_t ir_service_send(const ir_frame_t *frame, uint32_t carrier_hz)
{
  if (frame->count == 0 || frame->count > IR_FRAME_MAX_SYMBOLS) {
    return ESP_ERR_INVALID_ARG;
  }

  xSemaphoreTake(send_lock, portMAX_DELAY);

  /* A different carrier only for this frame, then back to the default. */
  esp_err_t err = carrier_hz ? ir_tx_set_carrier_hz(carrier_hz) : ESP_OK;
  if (err == ESP_OK) {
    tx_frame = *frame;
    err = send_locked();
  }
  ir_tx_set_carrier_hz(IR_CARRIER_HZ);

  xSemaphoreGive(send_lock);
  return err;
}

esp_err_t ir_service_send_nec(uint16_t address, uint8_t command, int repeats)
{
  if (repeats < 0 || repeats > 20) {
    return ESP_ERR_INVALID_ARG;
  }

  xSemaphoreTake(send_lock, portMAX_DELAY);

  TickType_t last_start = xTaskGetTickCount();
  ir_nec_encode(address, command, &tx_frame);
  esp_err_t err = send_locked();

  for (int i = 0; i < repeats && err == ESP_OK; i++) {
    xTaskDelayUntil(&last_start, pdMS_TO_TICKS(NEC_FRAME_PERIOD_MS));
    ir_nec_encode_repeat(&tx_frame);
    err = send_locked();
  }

  xSemaphoreGive(send_lock);
  return err;
}

esp_err_t ir_service_led_test(int seconds)
{
  xSemaphoreTake(send_lock, portMAX_DELAY);
  sending = true;
  esp_err_t err = ir_tx_led_test(seconds);
  send_end_us = esp_timer_get_time();
  sending = false;
  xSemaphoreGive(send_lock);
  return err;
}

bool ir_service_last(ir_frame_t *out)
{
  xSemaphoreTake(last_lock, portMAX_DELAY);
  *out = last_frame;
  xSemaphoreGive(last_lock);
  return out->count > 0;
}
