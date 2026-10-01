/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * hub_link: the connection to the hub (#42), as PROTOCOL.md describes it.
 *
 *   - announces the blaster on the network (mDNS, _uc-irblaster._tcp)
 *   - accepts the hub's mutual-TLS connection on port 7443
 *   - pairing: checks the hub's proof of the pairing code, pins the hub
 *   - carries out the hub's requests (send, learn, unpair, ...) and tells
 *     the hub about remotes it hears
 */

#pragma once

#include <stdbool.h>
#include "esp_err.h"

/* The TCP port the blaster listens on. */
#define HUB_LINK_PORT 7443

/* Starts mDNS and the server task. Call once, after wifi_start(),
 * identity_init() and ir_service_start(). */
esp_err_t hub_link_start(void);

/* A hub is connected right now (and authorised). */
bool hub_link_connected(void);

/* Pairing was changed outside the hub connection (console `unpair`):
 * update the mDNS announcement and drop the current connection. */
void hub_link_pairing_changed(void);
