/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * sensor_link: the sensor's own part of the hub connection (#86) -- the
 * request "read" and the event "reading" (../PROTOCOL.md).
 */

#pragma once

#include "esp_err.h"

/* Starts the hub connection (uc_addon's hub_link) with the sensor's
 * requests. Call once, after wifi_start(), identity_init() and
 * sensor_start(). */
esp_err_t sensor_link_start(void);
