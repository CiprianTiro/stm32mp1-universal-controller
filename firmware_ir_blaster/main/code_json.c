/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * Codes <-> JSON -- see code_json.h.
 */

#include "code_json.h"

#include <string.h>
#include "ir_nec.h"
#include "ir_tx.h"

/* Limits from PROTOCOL.md. */
#define RAW_MAX_TIMINGS (IR_FRAME_MAX_SYMBOLS * 2)
#define CARRIER_MIN_HZ 30000
#define CARRIER_MAX_HZ 60000

cJSON *code_to_json(const ir_frame_t *frame)
{
  cJSON *json = cJSON_CreateObject();
  if (!json) {
    return NULL;
  }

  ir_nec_result_t nec;
  if (ir_nec_decode(frame, &nec) == IR_NEC_FRAME) {
    cJSON_AddStringToObject(json, "proto", "nec");
    cJSON_AddNumberToObject(json, "address", nec.address);
    cJSON_AddNumberToObject(json, "command", nec.command);
    return json;
  }

  /* Raw: mark, space, mark, space, ... The last space is 0 ("end of
   * frame") and left out. The receiver measures the signal after the
   * receiver chip removed the carrier, so it can't tell the frequency;
   * almost every remote uses 38 kHz. */
  cJSON_AddStringToObject(json, "proto", "raw");
  cJSON_AddNumberToObject(json, "carrier_hz", IR_CARRIER_HZ);
  cJSON *timings = cJSON_AddArrayToObject(json, "timings");
  for (size_t i = 0; timings && i < frame->count; i++) {
    cJSON_AddItemToArray(timings, cJSON_CreateNumber(frame->symbols[i].duration0));
    if (frame->symbols[i].duration1) {
      cJSON_AddItemToArray(timings, cJSON_CreateNumber(frame->symbols[i].duration1));
    }
  }
  return json;
}

/* A whole number in [min, max], or false. */
static bool get_int(const cJSON *json, const char *name, double min, double max, double *out)
{
  const cJSON *item = cJSON_GetObjectItemCaseSensitive(json, name);
  if (!cJSON_IsNumber(item) || item->valuedouble != (double)(int64_t)item->valuedouble ||
      item->valuedouble < min || item->valuedouble > max) {
    return false;
  }
  *out = item->valuedouble;
  return true;
}

esp_err_t code_from_json(const cJSON *json, code_t *code, ir_frame_t *frame, const char **why)
{
  const cJSON *proto = cJSON_GetObjectItemCaseSensitive(json, "proto");
  double value;

  if (!cJSON_IsObject(json) || !cJSON_IsString(proto)) {
    *why = "code needs \"proto\"";
    return ESP_ERR_INVALID_ARG;
  }

  if (strcmp(proto->valuestring, "nec") == 0) {
    code->is_nec = true;
    if (!get_int(json, "address", 0, 0xFFFF, &value)) {
      *why = "nec address must be 0-65535";
      return ESP_ERR_INVALID_ARG;
    }
    code->address = (uint16_t)value;
    if (!get_int(json, "command", 0, 0xFF, &value)) {
      *why = "nec command must be 0-255";
      return ESP_ERR_INVALID_ARG;
    }
    code->command = (uint8_t)value;
    return ESP_OK;
  }

  if (strcmp(proto->valuestring, "raw") == 0) {
    code->is_nec = false;
    code->carrier_hz = IR_CARRIER_HZ;
    if (cJSON_GetObjectItemCaseSensitive(json, "carrier_hz")) {
      if (!get_int(json, "carrier_hz", CARRIER_MIN_HZ, CARRIER_MAX_HZ, &value)) {
        *why = "raw carrier_hz must be 30000-60000";
        return ESP_ERR_INVALID_ARG;
      }
      code->carrier_hz = (uint32_t)value;
    }

    const cJSON *timings = cJSON_GetObjectItemCaseSensitive(json, "timings");
    int n = cJSON_GetArraySize(timings);
    if (!cJSON_IsArray(timings) || n < 1 || n > RAW_MAX_TIMINGS) {
      *why = "raw timings: 1-1024 numbers";
      return ESP_ERR_INVALID_ARG;
    }

    /* Pairs of (mark, space) -> symbols; an odd count ends with a mark
     * whose space is 0 (end of frame). */
    int i = 0;
    const cJSON *t;
    frame->count = 0;
    cJSON_ArrayForEach(t, timings) {
      if (!cJSON_IsNumber(t) || t->valuedouble < 1 || t->valuedouble > IR_MAX_DURATION_US) {
        *why = "raw timings must be 1-32767 us each";
        return ESP_ERR_INVALID_ARG;
      }
      uint16_t us = (uint16_t)t->valuedouble;
      if (i % 2 == 0) {
        frame->symbols[frame->count] = ir_symbol(us, 0);
      } else {
        frame->symbols[frame->count++].duration1 = us;
      }
      i++;
    }
    if (i % 2 == 1) {
      frame->count++; /* the last mark, space 0 */
    }
    return ESP_OK;
  }

  *why = "proto must be \"nec\" or \"raw\"";
  return ESP_ERR_INVALID_ARG;
}
