/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * IR link: the blaster's own requests on the hub connection -- see
 * ir_link.h and PROTOCOL.md
 * ===========================================================================
 *
 * The connection itself (TLS, pairing, hello, ping, unpair) is shared with
 * the other add-ons (firmware_common/uc_addon/hub_link.c, since #86). This
 * file is what only a blaster does:
 *
 *   send    an IR code goes out
 *   learn   wait for a remote's button, reply with its code
 *   heard   (event) a remote was pressed while nobody was learning
 *
 * Everything here runs in hub_link's task (its hooks), except on_heard(),
 * which the IR receiver's task calls: IR frames reach hub_link's task
 * through a queue -- the receiver puts them in, poll() takes them out.
 */

#include "ir_link.h"

#include <string.h>

#include "code_json.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "hub_link.h"
#include "ir_nec.h"
#include "ir_service.h"

static const char *TAG = "ir_link";

#define LEARN_DEFAULT_MS 15000
#define LEARN_MIN_MS 1000
#define LEARN_MAX_MS 60000

/* Frames heard, waiting for hub_link's task. A few are plenty: a person
 * presses buttons slower than the loop turns. */
#define HEARD_QUEUE_LEN 4

static QueueHandle_t heard_queue;

/* A learn request waiting for a remote press (one connection at a time,
 * so one of these). */
static bool learning;
static double learn_id;
static int64_t learn_deadline_us;

/* Big buffers, static so they're not on the task's stack. Only hub_link's
 * task uses them. */
static ir_frame_t heard_frame;
static ir_frame_t request_frame;

/* ---------------------------------------------------------------------------
 * Requests
 * ------------------------------------------------------------------------- */

static bool op_send(hub_session_t *s, double id, const cJSON *req)
{
  code_t code;
  const char *why = NULL;
  if (code_from_json(cJSON_GetObjectItemCaseSensitive(req, "code"), &code, &request_frame, &why) != ESP_OK) {
    return hub_link_reply_error(s, id, "bad_request", why);
  }

  int repeats = 0;
  const cJSON *r = cJSON_GetObjectItemCaseSensitive(req, "repeats");
  if (r) {
    if (!cJSON_IsNumber(r) || r->valuedouble < 0 || r->valuedouble > 20 || !code.is_nec) {
      return hub_link_reply_error(s, id, "bad_request", "repeats: 0-20, NEC codes only");
    }
    repeats = (int)r->valuedouble;
  }

  esp_err_t err = code.is_nec ? ir_service_send_nec(code.address, code.command, repeats)
                              : ir_service_send(&request_frame, code.carrier_hz);
  if (err != ESP_OK) {
    return hub_link_reply_error(s, id, "failed", esp_err_to_name(err));
  }
  return hub_link_send(s, hub_link_reply_ok(id));
}

static bool op_learn(hub_session_t *s, double id, const cJSON *req)
{
  if (learning) {
    return hub_link_reply_error(s, id, "busy", "already learning");
  }

  double timeout_ms = LEARN_DEFAULT_MS;
  const cJSON *t = cJSON_GetObjectItemCaseSensitive(req, "timeout_ms");
  if (t) {
    if (!cJSON_IsNumber(t) || t->valuedouble < LEARN_MIN_MS || t->valuedouble > LEARN_MAX_MS) {
      return hub_link_reply_error(s, id, "bad_request", "timeout_ms: 1000-60000");
    }
    timeout_ms = t->valuedouble;
  }

  /* Only a press from NOW on counts: forget frames already waiting. */
  xQueueReset(heard_queue);
  learning = true;
  learn_id = id;
  learn_deadline_us = esp_timer_get_time() + (int64_t)(timeout_ms * 1000);
  ESP_LOGI(TAG, "learning: press a button on the remote");
  return true; /* the reply comes when a frame arrives (poll) */
}

static bool handle(hub_session_t *s, const char *op, double id, const cJSON *req, bool *handled)
{
  *handled = true;
  if (strcmp(op, "send") == 0) {
    return op_send(s, id, req);
  }
  if (strcmp(op, "learn") == 0) {
    return op_learn(s, id, req);
  }
  *handled = false;
  return true;
}

/* ---------------------------------------------------------------------------
 * IR frames heard -> the hub
 * ------------------------------------------------------------------------- */

/* ir_service listener, in the RECEIVE task: only queue, never block. */
static void on_heard(const ir_frame_t *frame, bool echo)
{
  if (echo || !hub_link_connected()) {
    return;
  }
  xQueueSend(heard_queue, frame, 0); /* full: drop it, the hub missed one press */
}

/* In hub_link's task: frames from the queue become the learn reply, or
 * "heard" events. */
static bool poll(hub_session_t *s)
{
  while (xQueueReceive(heard_queue, &heard_frame, 0) == pdTRUE) {
    /* An NEC "button held" repeat carries no code: nothing to learn or
     * report. */
    ir_nec_result_t nec;
    if (ir_nec_decode(&heard_frame, &nec) == IR_NEC_REPEAT) {
      continue;
    }

    cJSON *code = code_to_json(&heard_frame);
    cJSON *msg;
    if (learning) {
      learning = false;
      msg = hub_link_reply_ok(learn_id);
      ESP_LOGI(TAG, "learned a code");
    } else {
      msg = cJSON_CreateObject();
      cJSON_AddStringToObject(msg, "event", "heard");
    }
    cJSON_AddItemToObject(msg, "code", code);
    if (!hub_link_send(s, msg)) {
      return false;
    }
  }

  if (learning && esp_timer_get_time() > learn_deadline_us) {
    learning = false;
    return hub_link_reply_error(s, learn_id, "timeout", "no remote was pressed");
  }
  return true;
}

/* A new connection doesn't inherit the old one's learn. */
static void session_changed(bool active)
{
  learning = false;
}

static const uc_addon_app_t ir_app = {
  .handle = handle,
  .poll = poll,
  .session_changed = session_changed,
};

esp_err_t ir_link_start(void)
{
  heard_queue = xQueueCreate(HEARD_QUEUE_LEN, sizeof(ir_frame_t));
  if (!heard_queue) {
    return ESP_ERR_NO_MEM;
  }
  ir_service_add_listener(on_heard);
  return hub_link_start(&ir_app);
}
