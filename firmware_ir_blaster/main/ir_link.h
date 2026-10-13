/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ir_link: the blaster's own part of the hub connection (#42; split from
 * hub_link.c for #86) -- the requests send and learn, and the "heard"
 * event (PROTOCOL.md).
 */

#pragma once

#include "esp_err.h"

/* Starts the hub connection (uc_addon's hub_link) with the blaster's
 * requests. Call once, after wifi_start(), identity_init() and
 * ir_service_start(). */
esp_err_t ir_link_start(void);
