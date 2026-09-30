/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ir_tx: sends IR frames through the IR LED (#42).
 *
 * Hardware (see the wiki page IR-Blaster-Hardware): the GPIO drives an NPN
 * transistor through 1 kOhm, the transistor switches the IR LED's current
 * from 5V. GPIO high = LED on. The RMT peripheral adds the 38 kHz carrier
 * itself: during a "mark" it toggles the pin 38,000 times a second, during
 * a "space" it holds the pin low.
 */

#pragma once

#include "esp_err.h"
#include "driver/gpio.h"
#include "ir_frame.h"

/* The carrier almost every consumer IR receiver is tuned to (our CHQ1838
 * too), used unless a code asks for another. */
#define IR_CARRIER_HZ 38000

/* Sets up the RMT transmit channel on `gpio`. Call once at start. */
esp_err_t ir_tx_init(gpio_num_t gpio);

/* Sends `frame` and waits until it has gone out (a few tens of ms). */
esp_err_t ir_tx_send(const ir_frame_t *frame);

/* Changes the carrier frequency (30-60 kHz) for the following sends, e.g.
 * for a learned raw code from a 36 kHz remote. Default IR_CARRIER_HZ. */
esp_err_t ir_tx_set_carrier_hz(uint32_t hz);

/* The carrier frequency currently in use. */
uint32_t ir_tx_carrier_hz(void);

/* Changes the carrier's duty cycle: the share of each 38 kHz cycle the LED
 * is on, 10-50 %. More = more light per mark, but receivers are designed
 * for roughly 25-50 %. Default IR_CARRIER_DUTY_PCT. */
esp_err_t ir_tx_set_duty(int percent);

/* Hardware check: keeps the LED on steadily -- no 38 kHz blinking -- for
 * `seconds` (1-20), so the LED circuit can be measured with a multimeter
 * and seen through a phone camera. Blocks until done. */
esp_err_t ir_tx_led_test(int seconds);
