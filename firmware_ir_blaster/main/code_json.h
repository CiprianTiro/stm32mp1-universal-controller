/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * code_json: IR codes as the hub sees them, in JSON (#42).
 *
 * Inside the firmware a code is an ir_frame_t (mark/space symbols). Towards
 * the hub it's one of two JSON shapes (PROTOCOL.md, "Codes"):
 *
 *   {"proto": "nec", "address": 0, "command": 64}
 *   {"proto": "raw", "carrier_hz": 38000, "timings": [9000, 4500, 560, ...]}
 *
 * NEC is short and readable; raw works for every remote.
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>
#include "cJSON.h"
#include "esp_err.h"
#include "ir_frame.h"

/* A code parsed from JSON: either NEC (address/command) or a raw frame. */
typedef struct {
  bool is_nec;
  uint16_t address;     /* NEC */
  uint8_t command;      /* NEC */
  uint32_t carrier_hz;  /* raw */
} code_t;

/* The JSON for a frame heard: "nec" if it decodes as NEC, else "raw".
 * NULL if out of memory. Caller frees with cJSON_Delete. */
cJSON *code_to_json(const ir_frame_t *frame);

/* Reads a code. For raw codes the frame is filled into `frame`. On error,
 * `why` says what's wrong (for the hub's log). */
esp_err_t code_from_json(const cJSON *json, code_t *code, ir_frame_t *frame, const char **why);
