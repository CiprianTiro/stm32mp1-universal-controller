/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * ir_service: one place that owns the IR hardware (#42)
 * ===========================================================================
 *
 * Two parts of the firmware use IR: the serial console (typed commands)
 * and the hub connection (hub_link.c). Both go through here, so that:
 *
 *   - only one frame is sent at a time (a mutex), whoever asks;
 *   - echoes of our own sends are recognised in ONE place: the receiver
 *     hears the blaster's LEDs too (directly or off a wall), and such a
 *     frame must never count as "a remote was pressed";
 *   - every frame heard is passed to all interested parts ("listeners").
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>
#include "driver/gpio.h"
#include "esp_err.h"
#include "ir_frame.h"

/* Called from the receive task for every frame heard (noise already
 * dropped). `echo`: it arrived while or right after we sent, so it's most
 * likely our own LEDs. Must return quickly (copy, queue, print). */
typedef void (*ir_listener_t)(const ir_frame_t *frame, bool echo);

/* Starts the transmitter and the receiver. Call once. */
esp_err_t ir_service_start(gpio_num_t tx_gpio, gpio_num_t rx_gpio);

/* Adds a listener (up to 4). */
void ir_service_add_listener(ir_listener_t listener);

/* Sends a frame at `carrier_hz` (0 = the default 38 kHz). Blocks until
 * it has gone out; waits if another send is in progress. */
esp_err_t ir_service_send(const ir_frame_t *frame, uint32_t carrier_hz);

/* Sends an NEC code, then `repeats` "button held" frames (0-20), 108 ms
 * apart, as a remote does while a button is held. */
esp_err_t ir_service_send_nec(uint16_t address, uint8_t command, int repeats);

/* Hardware check: LEDs steadily on for `seconds` (ir_tx_led_test), taking
 * the send lock so no frame goes out meanwhile. */
esp_err_t ir_service_led_test(int seconds);

/* The last frame heard that wasn't an echo (or an NEC repeat). Returns
 * false if nothing was heard yet. */
bool ir_service_last(ir_frame_t *out);
