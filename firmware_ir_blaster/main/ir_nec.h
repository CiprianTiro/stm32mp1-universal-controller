/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * ir_nec: the NEC IR protocol, encode and decode (#42)
 * ===========================================================================
 *
 * NEC is the most common consumer IR protocol: LG TVs, most cheap IR LED
 * strips (24/44-key remotes), many audio devices. One frame looks like:
 *
 *   9000 us mark + 4500 us space              "start" (leader)
 *   32 bits, each: 560 us mark + a space      560 us space = 0, 1690 us = 1
 *   560 us mark                               "stop" (so the last space ends)
 *
 * The 32 bits are 4 bytes, each sent least-significant bit first:
 *
 *   standard NEC:  address, ~address, command, ~command
 *   extended NEC:  address low byte, address high byte, command, ~command
 *
 * (~x = x with every bit flipped: a simple check that the byte arrived
 * intact). Holding a button sends a short "repeat" frame instead
 * (9000 mark + 2250 space + 560 mark) about every 108 ms.
 *
 * Example: LG TV power = address 0x04, command 0x08. Websites often write
 * the same code as the 32-bit number 0x20DF10EF -- that's the bits in the
 * order they're sent, first bit as the highest. We print both.
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>
#include "ir_frame.h"

typedef enum {
  IR_NEC_NONE = 0, /* not an NEC frame */
  IR_NEC_FRAME,    /* a full frame: address + command are valid */
  IR_NEC_REPEAT,   /* "button still held" repeat frame, no data */
} ir_nec_kind_t;

typedef struct {
  ir_nec_kind_t kind;
  uint16_t address;  /* 8-bit for standard NEC, 16-bit for extended */
  bool extended;     /* true if the second byte wasn't ~address */
  uint8_t command;
  uint32_t raw_msb;  /* the 32 bits in sending order, first bit = bit 31 */
} ir_nec_result_t;

/* Fills `frame` with an NEC frame. Addresses 0x00-0xFF are sent as
 * standard NEC (address, ~address), larger ones as extended NEC. */
void ir_nec_encode(uint16_t address, uint8_t command, ir_frame_t *frame);

/* Fills `frame` with an NEC "repeat" frame: what a remote sends every
 * 108 ms while a button is held (9000 us mark, 2250 us space, 560 us mark). */
void ir_nec_encode_repeat(ir_frame_t *frame);

/* Tries to read `frame` as NEC. Returns the kind found (IR_NEC_NONE if it
 * isn't NEC, or the command check byte doesn't match). */
ir_nec_kind_t ir_nec_decode(const ir_frame_t *frame, ir_nec_result_t *out);
