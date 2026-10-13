/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * hub_link: the connection to the hub (#42; shared by the add-ons since
 * #86), as firmware_ir_blaster/PROTOCOL.md describes it.
 *
 *   - announces the add-on on the network (mDNS, uc_addon.h's service)
 *   - accepts the hub's mutual-TLS connection on port 7443
 *   - pairing: checks the hub's proof of the pairing code, pins the hub
 *   - answers hello, pair, ping and unpair itself, and hands every other
 *     request to the add-on's own code (uc_addon_app_t below): the
 *     blaster's send / learn, the sensor's read. The add-on's code can
 *     also send events to the hub (the blaster's "heard", the sensor's
 *     "reading").
 *
 * Everything runs in ONE task (hub_link.c says why), so the add-on's
 * hooks are called from that task, and may only write to the hub from
 * there -- that's what the session pointer they get is for.
 */

#pragma once

#include <stdbool.h>
#include "cJSON.h"
#include "esp_err.h"

/* The TCP port the add-on listens on. */
#define HUB_LINK_PORT 7443

/* One hub connection (opaque). Only valid during a hook call. */
typedef struct hub_session hub_session_t;

/* The add-on's part of the conversation. Any hook may be NULL. */
typedef struct {
  /* A request from the paired hub that hub_link doesn't answer itself
   * (`op`, with its `id`). Set `*handled` = false if the add-on doesn't
   * know it either (the hub gets "unknown op"). The reply may come now
   * (hub_link_reply_*) or later, from poll() (the blaster's learn waits
   * for a remote). Returns false to close the connection. */
  bool (*handle)(hub_session_t *s, const char *op, double id, const cJSON *req, bool *handled);
  /* Called about every 100 ms while the paired hub is connected: send
   * events, late replies, time-outs. Returns false to close. */
  bool (*poll)(hub_session_t *s);
  /* The paired hub connected (true) or the connection ended (false):
   * reset per-connection state. */
  void (*session_changed)(bool active);
} uc_addon_app_t;

/* Starts mDNS and the server task. Call once, after wifi_start() and
 * identity_init(); `app` must stay valid for good (a static const). */
esp_err_t hub_link_start(const uc_addon_app_t *app);

/* A hub is connected right now (and authorised). */
bool hub_link_connected(void);

/* Pairing was changed outside the hub connection (console `unpair`):
 * update the mDNS announcement and drop the current connection. */
void hub_link_pairing_changed(void);

/* ---- For the hooks (hub_link's task only) ---- */

/* {"id": <id>, "ok": true}: add fields, then hub_link_send() it. */
cJSON *hub_link_reply_ok(double id);

/* {"id": <id>, "ok": false, "error": <kind>, "detail": <detail>}, sent.
 * Returns false if the connection broke. */
bool hub_link_reply_error(hub_session_t *s, double id, const char *kind, const char *detail);

/* Sends `msg` (a reply or an event) as one line, and frees it. Returns
 * false if the connection broke. */
bool hub_link_send(hub_session_t *s, cJSON *msg);
