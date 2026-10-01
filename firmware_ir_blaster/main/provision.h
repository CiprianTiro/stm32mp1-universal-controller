/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * provision: WiFi setup over Bluetooth (#42, the way issue #71 plans it)
 * ===========================================================================
 *
 * A blaster without a WiFi network (new, or after holding BOOT for 5 s)
 * announces itself over Bluetooth Low Energy as "PROV_IRB_xxxx". The hub
 * (or Espressif's esp_prov tool / phone app, for testing) connects, proves
 * it knows the PAIRING CODE, and sends the home WiFi's name and password.
 * The blaster joins the WiFi, reports whether it worked, and switches
 * Bluetooth off.
 *
 * This is Espressif's standard "network provisioning" with security level
 * 2: SRP6a, a password-authenticated key exchange, with
 *   username  "wifiprov"      (Espressif's default, so their tools work)
 *   password  the pairing code without dashes, upper case (identity.c)
 * Both sides derive the same encryption key only if both used the same
 * code, and the code itself never goes over the air. Someone listening
 * learns nothing; someone pretending gets one guess per attempt. The WiFi
 * password then travels encrypted and tamper-proof (AES-GCM).
 *
 * So the same code protects both steps: this one (Bluetooth), and pairing
 * with the hub afterwards over WiFi (hub_link.c, PROTOCOL.md).
 */

#pragma once

#include <stdbool.h>
#include "esp_err.h"

/* Starts Bluetooth setup if no WiFi network is configured; otherwise
 * only releases the Bluetooth memory. Returns whether setup is running.
 * Call once, after esp_wifi_init() (wifi.c does). */
esp_err_t provision_start_if_needed(bool *started);

/* Bluetooth setup is running right now (until the WiFi works). */
bool provision_active(void);

/* Stops a running setup (e.g. the network was set on the console). */
void provision_stop(void);

/* Watches the BOOT button: held for 5 s = forget the WiFi and restart
 * into Bluetooth setup. Call once at start. */
esp_err_t provision_watch_button(void);
