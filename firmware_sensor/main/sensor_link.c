/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * Sensor link -- see sensor_link.h and ../PROTOCOL.md
 * ===========================================================================
 *
 * The connection itself (TLS, pairing, hello, ping, unpair) is shared with
 * the other add-ons (firmware_common/uc_addon/hub_link.c). This file adds
 * what only the sensor does:
 *
 *   read      (request) the latest reading, at once
 *   reading   (event) a new reading -- sent whenever sensor.c has one
 *             (every minute), and right after the hub connects, so the
 *             hub never waits a minute to show something
 *
 * Everything here runs in hub_link's task (its hooks); sensor.c's task
 * only measures, and sensor_latest() hands its result over safely.
 */

#include "sensor_link.h"

#include <math.h>
#include <string.h>
#include "esp_timer.h"
#include "hub_link.h"
#include "sensor.h"

/* The changes counter of the reading the hub has (sensor_latest). */
static uint32_t sent;
/* The hub has just connected: send the latest reading, new or not. */
static bool send_now;

/* Adds a reading's fields to a message (PROTOCOL.md):
 *   "sensor": "sht21"  (""  = no sensor found)
 *   "temperature": 21.43, "humidity": 48.2  (only when there's a reading)
 *   "age_s": 12  (how old it is)
 * Rounded to what the sensors can really tell apart. */
static void add_reading(cJSON *msg, const sensor_reading_t *r)
{
  cJSON_AddStringToObject(msg, "sensor", r->sensor);
  if (r->valid) {
    cJSON_AddNumberToObject(msg, "temperature", round(r->temperature_c * 100.0) / 100.0);
    cJSON_AddNumberToObject(msg, "humidity", round(r->humidity_pct * 10.0) / 10.0);
    cJSON_AddNumberToObject(msg, "age_s", (double)((esp_timer_get_time() - r->at_us) / 1000000));
  }
}

static bool handle(hub_session_t *s, const char *op, double id, const cJSON *req, bool *handled)
{
  if (strcmp(op, "read") != 0) {
    *handled = false;
    return true;
  }
  *handled = true;
  sensor_reading_t r;
  sent = sensor_latest(&r);
  cJSON *reply = hub_link_reply_ok(id);
  add_reading(reply, &r);
  return hub_link_send(s, reply);
}

static bool poll(hub_session_t *s)
{
  sensor_reading_t r;
  uint32_t changes = sensor_latest(&r);
  if (changes == sent && !send_now) {
    return true;
  }
  sent = changes;
  send_now = false;
  cJSON *event = cJSON_CreateObject();
  cJSON_AddStringToObject(event, "event", "reading");
  add_reading(event, &r);
  return hub_link_send(s, event);
}

static void session_changed(bool active)
{
  send_now = active;
}

static const uc_addon_app_t sensor_app = {
  .handle = handle,
  .poll = poll,
  .session_changed = session_changed,
};

esp_err_t sensor_link_start(void)
{
  return hub_link_start(&sensor_app);
}
