/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * IR transmit -- see ir_tx.h.
 */

#include "ir_tx.h"

#include "driver/rmt_tx.h"
#include "esp_check.h"

static const char *TAG = "ir_tx";

/* Carrier: 38 kHz is what almost every consumer receiver is tuned to
 * (including our CHQ1838). The duty cycle is how much of each carrier
 * period the LED is on. 33 % is the usual remote-control choice (LED and
 * transistor stay cooler); 50 % gives more light per mark and receivers
 * accept it too. Changeable at run time with ir_tx_set_duty(). The
 * frequency (IR_CARRIER_HZ, ir_tx.h) with ir_tx_set_carrier_hz(). */
#define IR_CARRIER_DUTY_PCT 33

/* How long ir_tx_send() waits for a frame to finish. The longest frame
 * (512 symbols of up to 2 x 32.8 ms) can't take longer than this in
 * practice; normal frames take 50-150 ms. */
#define IR_TX_TIMEOUT_MS 2000

static rmt_channel_handle_t tx_channel;

static rmt_carrier_config_t carrier = {
  .frequency_hz = IR_CARRIER_HZ,
  .duty_cycle = IR_CARRIER_DUTY_PCT / 100.0f,
  /* Carrier on the "high" level = during marks (our level 1). */
  .flags.polarity_active_low = 0,
};

/* The "copy encoder" passes our symbols to the hardware unchanged. (RMT
 * encoders can also build symbols from bytes; we already have symbols.) */
static rmt_encoder_handle_t copy_encoder;

esp_err_t ir_tx_init(gpio_num_t gpio)
{
  rmt_tx_channel_config_t config = {
    .gpio_num = gpio,
    .clk_src = RMT_CLK_SRC_DEFAULT,
    .resolution_hz = IR_RESOLUTION_HZ,
    /* Hardware buffer: 2 blocks of 48 symbols. Longer frames still work --
     * the driver refills the buffer while it's sending ("ping-pong"). */
    .mem_block_symbols = 96,
    .trans_queue_depth = 4,
    /* Start with the pin low (LED off). The 10 kOhm pull-down on the
     * transistor's base covers the time before this code runs. */
    .flags.init_level = 0,
  };
  ESP_RETURN_ON_ERROR(rmt_new_tx_channel(&config, &tx_channel), TAG, "create TX channel");

  ESP_RETURN_ON_ERROR(rmt_apply_carrier(tx_channel, &carrier), TAG, "apply carrier");

  rmt_copy_encoder_config_t encoder_config = { };
  ESP_RETURN_ON_ERROR(rmt_new_copy_encoder(&encoder_config, &copy_encoder), TAG, "create encoder");

  ESP_RETURN_ON_ERROR(rmt_enable(tx_channel), TAG, "enable TX channel");
  ESP_LOGI(TAG, "IR transmitter ready on GPIO%d (%d Hz carrier)", gpio, IR_CARRIER_HZ);
  return ESP_OK;
}

esp_err_t ir_tx_send(const ir_frame_t *frame)
{
  if (frame->count == 0) {
    return ESP_ERR_INVALID_ARG;
  }

  rmt_transmit_config_t tx_config = {
    .loop_count = 0,        /* send once */
    .flags.eot_level = 0,   /* afterwards: pin low, LED off */
  };
  ESP_RETURN_ON_ERROR(rmt_transmit(tx_channel, copy_encoder, frame->symbols,
                                   frame->count * sizeof(rmt_symbol_word_t), &tx_config),
                      TAG, "transmit");

  /* rmt_transmit() only queues the frame; the data must stay untouched
   * until it has been sent, so wait here. */
  return rmt_tx_wait_all_done(tx_channel, IR_TX_TIMEOUT_MS);
}

esp_err_t ir_tx_led_test(int seconds)
{
  /* Static: 2 KB, and the hardware reads it while sending. */
  static ir_frame_t steady;

  if (seconds < 1 || seconds > 20) {
    return ESP_ERR_INVALID_ARG;
  }

  /* One symbol = two halves of the longest duration, both "on":
   * 2 x 32767 us = ~65.5 ms of LED on. */
  const uint32_t symbol_us = 2 * IR_MAX_DURATION_US;
  size_t count = ((uint32_t)seconds * 1000000 + symbol_us - 1) / symbol_us;
  for (size_t i = 0; i < count; i++) {
    steady.symbols[i] = (rmt_symbol_word_t) {
      .level0 = 1, .duration0 = IR_MAX_DURATION_US,
      .level1 = 1, .duration1 = IR_MAX_DURATION_US,
    };
  }
  steady.count = count;

  /* Carrier off: a "mark" is now simply the pin held high = LED fully on.
   * Put it back afterwards whatever happened, or every later send would
   * go out without the 38 kHz carrier and no receiver would see it. */
  ESP_RETURN_ON_ERROR(rmt_apply_carrier(tx_channel, NULL), TAG, "carrier off");

  rmt_transmit_config_t tx_config = { .loop_count = 0, .flags.eot_level = 0 };
  esp_err_t err = rmt_transmit(tx_channel, copy_encoder, steady.symbols,
                               steady.count * sizeof(rmt_symbol_word_t), &tx_config);
  if (err == ESP_OK) {
    err = rmt_tx_wait_all_done(tx_channel, seconds * 1000 + 1000);
  }

  esp_err_t carrier_err = rmt_apply_carrier(tx_channel, &carrier);
  return err != ESP_OK ? err : carrier_err;
}

esp_err_t ir_tx_set_duty(int percent)
{
  if (percent < 10 || percent > 50) {
    return ESP_ERR_INVALID_ARG;
  }
  carrier.duty_cycle = percent / 100.0f;
  return rmt_apply_carrier(tx_channel, &carrier);
}

esp_err_t ir_tx_set_carrier_hz(uint32_t hz)
{
  if (hz < 30000 || hz > 60000) {
    return ESP_ERR_INVALID_ARG;
  }
  if (hz == carrier.frequency_hz) {
    return ESP_OK;
  }
  carrier.frequency_hz = hz;
  return rmt_apply_carrier(tx_channel, &carrier);
}

uint32_t ir_tx_carrier_hz(void)
{
  return carrier.frequency_hz;
}
