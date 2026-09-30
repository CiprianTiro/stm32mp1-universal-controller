/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ir_rx: listens to the IR receiver and hands over every frame it hears
 * (#42).
 *
 * Hardware: the CHQ1838 receiver's OUT pin, powered from 3.3 V. The
 * receiver does the hard part itself: it filters out everything that
 * isn't a 38 kHz carrier and strips the carrier off, so OUT simply goes LOW
 * during a mark and stays HIGH during a space. We measure how long each
 * LOW and HIGH lasts.
 */

#pragma once

#include "esp_err.h"
#include "driver/gpio.h"
#include "ir_frame.h"

/* Called from the receive task for every frame heard. `frame` is only
 * valid during the call; copy it to keep it. */
typedef void (*ir_rx_handler_t)(const ir_frame_t *frame);

/* Sets up the RMT receive channel on `gpio` and starts a task that
 * listens forever, calling `handler` for each frame. */
esp_err_t ir_rx_start(gpio_num_t gpio, ir_rx_handler_t handler);
