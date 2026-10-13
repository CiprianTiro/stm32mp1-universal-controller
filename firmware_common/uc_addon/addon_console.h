/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * addon_console: the serial-console commands every add-on has (#42; shared since
 * #86). The add-on registers its own commands next to these.
 *
 *   status                     WiFi, hub connection, firmware version
 *   wifi <name> <password>     join a network (development; normally the
 *                              hub sets it over Bluetooth)
 *   wifi forget                forget it, restart into Bluetooth setup
 *   pairing                    show the pairing code the hub asks for
 *   unpair                     forget the hub
 *   factory-reset yes          forget everything, new identity
 */

#pragma once

#include "esp_err.h"

/* Registers the commands above with esp_console. Call once, after the
 * console was created (esp_console_new_repl_uart). */
esp_err_t uc_addon_console_register(void);
