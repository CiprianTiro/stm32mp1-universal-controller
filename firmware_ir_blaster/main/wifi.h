/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * wifi: joins the home network (#42).
 *
 * For now the network name and password are typed once into the serial
 * console (`wifi <name> <password>`); ESP-IDF keeps them in flash. Setting
 * them up over Bluetooth from the hub's wizard comes later (#71).
 *
 * Once configured, the blaster connects at every start and reconnects by
 * itself whenever the connection drops (router restart, out of range...).
 */

#pragma once

#include <stdbool.h>
#include "esp_err.h"

/* Starts WiFi; connects if a network is configured. Call once, after
 * nvs_flash_init(). */
esp_err_t wifi_start(void);

/* Saves a new network and connects to it. `password` may be "" for an
 * open network. */
esp_err_t wifi_set_network(const char *ssid, const char *password);

/* Forgets the network and disconnects. */
esp_err_t wifi_forget(void);

/* Connected and has an IP address. */
bool wifi_is_connected(void);

/* Prints the state: network name, IP address, signal strength. */
void wifi_print_status(void);
