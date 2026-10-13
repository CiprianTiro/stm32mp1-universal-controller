/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * wifi: joins the home network (#42; shared by the add-ons since #86).
 *
 * The network name and password come over Bluetooth from the hub's wizard
 * (provision.c), or -- for development -- from the serial console
 * (`wifi <name> <password>`); ESP-IDF keeps them in flash either way.
 *
 * Once configured, the add-on connects at every start and reconnects by
 * itself whenever the connection drops (router restart, out of range...).
 */

#pragma once

#include <stdbool.h>
#include "esp_err.h"

/* Starts WiFi: connects if a network is configured, else starts setup
 * over Bluetooth (provision.c). Call once, after nvs_flash_init() and
 * identity_init() (setup uses the pairing code). */
esp_err_t wifi_start(void);

/* Saves a new network and connects to it. `password` may be "" for an
 * open network. */
esp_err_t wifi_set_network(const char *ssid, const char *password);

/* Forgets the network and disconnects. (Bluetooth setup only runs after a
 * restart: its memory was released once the WiFi was set up.) */
esp_err_t wifi_forget(void);

/* Sets the WiFi power saving the add-on wants (uc_addon.h: none for the
 * blaster, modem sleep for the sensor) -- unless Bluetooth setup still
 * runs (it needs power saving on). provision.c calls it again when setup
 * has finished. */
void wifi_power_save_settle(void);

/* Connected and has an IP address. */
bool wifi_is_connected(void);

/* Prints the state: network name, IP address, signal strength. */
void wifi_print_status(void);
