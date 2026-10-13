/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * uc_addon: what every add-on of the hub has in common (#42, #86)
 * ===========================================================================
 *
 * The hub's own ESP32 add-ons -- the IR blaster (firmware_ir_blaster/) and
 * the room sensor (firmware_sensor/) -- are set up and reached the same
 * way. This component is that common part:
 *
 *   identity.c    who this add-on is: key, certificate, pairing code, and
 *                 which hub it belongs to (kept in flash)
 *   wifi.c        joins the home network, reconnects by itself
 *   provision.c   WiFi setup over Bluetooth (no network yet / BOOT held)
 *   hub_link.c    the encrypted, paired connection to the hub: hello,
 *                 pair, ping, unpair -- and the add-on's own requests,
 *                 handed to its firmware (hub_link.h, uc_addon_app_t)
 *
 * The add-ons differ only in NAMES (so the hub tells them apart: the mDNS
 * service, the Bluetooth service, the pairing label), in their WiFi power
 * saving, and in their own requests. The firmware describes itself ONCE,
 * at start, with uc_addon_init(), before anything else of this component.
 *
 * The IR blaster's names are the ones it had before this component
 * existed, so a blaster that's already paired keeps working with the hub.
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>

typedef struct {
  /* What a person calls it, in logs and the mDNS instance name: "IR
   * blaster", "Sensor" ("IR blaster 91e4" on the network). */
  const char *product;
  /* The device id's prefix: "irb" -> "irb-7ce8b1b091e4". */
  const char *id_prefix;
  /* The mDNS service the hub browses for: "_uc-irblaster". */
  const char *mdns_service;
  /* The Bluetooth setup name's prefix: "PROV_IRB_" -> "PROV_IRB_91E4". */
  const char *prov_prefix;
  /* The Bluetooth setup service UUID, little-endian (the byte order the
   * provisioning component expects). The hub's template names it too. */
  uint8_t ble_service_uuid[16];
  /* The pairing proof's label and this side's role in it (PROTOCOL.md):
   * HMAC(code, "<label> <role> FB FH"). */
  const char *pair_label;
  const char *pair_role;
  /* Reported in hello and mDNS. */
  const char *fw_version;
  int protocol_version;
  /* WiFi power saving once set up. false: none, every message answered at
   * once (the blaster: IR must go out the moment the hub says). true:
   * modem sleep, the radio wakes for each beacon (~100-300 ms later
   * answers, far less power and HEAT -- the sensor, which mustn't warm
   * itself). */
  bool wifi_power_save;
} uc_addon_config_t;

/* Describes the add-on. Call first in app_main(); `config` must stay
 * valid for good (a static const). */
void uc_addon_init(const uc_addon_config_t *config);

/* The description given to uc_addon_init(). */
const uc_addon_config_t *uc_addon_config(void);
