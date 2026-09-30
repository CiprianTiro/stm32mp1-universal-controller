/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * NEC encode/decode -- see ir_nec.h for what the protocol looks like.
 */

#include "ir_nec.h"

/* Nominal NEC timings, in microseconds. */
#define NEC_LEADER_MARK_US   9000
#define NEC_LEADER_SPACE_US  4500 /* full frame */
#define NEC_REPEAT_SPACE_US  2250 /* "button held" repeat frame */
#define NEC_BIT_MARK_US       560
#define NEC_ZERO_SPACE_US     560
#define NEC_ONE_SPACE_US     1690

/* Real receivers aren't exact: they stretch or shorten marks by 100-200 us
 * (our CHQ1838 gives 340-550 us for NEC's 560 us bit marks) and cheap
 * remotes drift too. Long parts (leader) get +-30 %; the short bit marks
 * +-50 %, since 200 us off is a lot on 560 us. The bit *spaces* carry the
 * data and are checked separately (0 vs 1 split at the midpoint). */
#define NEC_TOLERANCE_PCT          30
#define NEC_BIT_MARK_TOLERANCE_PCT 50

/* True if `measured` is within `pct` percent of `nominal`. */
static bool near(uint32_t measured, uint32_t nominal, uint32_t pct)
{
  uint32_t margin = nominal * pct / 100;
  return measured >= nominal - margin && measured <= nominal + margin;
}

void ir_nec_encode(uint16_t address, uint8_t command, ir_frame_t *frame)
{
  uint8_t bytes[4];

  if (address <= 0xFF) {
    bytes[0] = (uint8_t)address;
    bytes[1] = (uint8_t)~address; /* standard NEC: check byte */
  } else {
    bytes[0] = (uint8_t)(address & 0xFF); /* extended NEC: 16-bit address, */
    bytes[1] = (uint8_t)(address >> 8);   /* low byte first               */
  }
  bytes[2] = command;
  bytes[3] = (uint8_t)~command;

  size_t n = 0;
  frame->symbols[n++] = ir_symbol(NEC_LEADER_MARK_US, NEC_LEADER_SPACE_US);

  /* 4 bytes, each least-significant bit first. */
  for (int b = 0; b < 4; b++) {
    for (int bit = 0; bit < 8; bit++) {
      bool one = (bytes[b] >> bit) & 1;
      frame->symbols[n++] = ir_symbol(NEC_BIT_MARK_US,
                                      one ? NEC_ONE_SPACE_US : NEC_ZERO_SPACE_US);
    }
  }

  /* Stop mark, then a long space so back-to-back sends stay apart (the NEC
   * spec wants ~40 ms between frames; the RMT field maxes out at ~32.8 ms,
   * and the transmit code adds its own gap on top). */
  frame->symbols[n++] = ir_symbol(NEC_BIT_MARK_US, IR_MAX_DURATION_US);
  frame->count = n;
}

void ir_nec_encode_repeat(ir_frame_t *frame)
{
  frame->symbols[0] = ir_symbol(NEC_LEADER_MARK_US, NEC_REPEAT_SPACE_US);
  frame->symbols[1] = ir_symbol(NEC_BIT_MARK_US, IR_MAX_DURATION_US);
  frame->count = 2;
}

ir_nec_kind_t ir_nec_decode(const ir_frame_t *frame, ir_nec_result_t *out)
{
  out->kind = IR_NEC_NONE;
  if (frame->count < 2) {
    return IR_NEC_NONE;
  }

  const rmt_symbol_word_t *s = frame->symbols;

  /* Every NEC frame starts with the 9 ms leader mark. */
  if (s[0].level0 != 1 || !near(s[0].duration0, NEC_LEADER_MARK_US, NEC_TOLERANCE_PCT)) {
    return IR_NEC_NONE;
  }

  /* Leader + 2.25 ms space + stop mark = repeat frame. */
  if (near(s[0].duration1, NEC_REPEAT_SPACE_US, NEC_TOLERANCE_PCT) && frame->count <= 3) {
    out->kind = IR_NEC_REPEAT;
    return IR_NEC_REPEAT;
  }

  /* Full frame: leader space 4.5 ms, then 32 bit symbols, then the stop
   * mark = at least 34 symbols. */
  if (!near(s[0].duration1, NEC_LEADER_SPACE_US, NEC_TOLERANCE_PCT) || frame->count < 34) {
    return IR_NEC_NONE;
  }

  uint8_t bytes[4] = { 0 };
  uint32_t raw_msb = 0;

  for (int i = 0; i < 32; i++) {
    const rmt_symbol_word_t *bit = &s[1 + i];
    if (!near(bit->duration0, NEC_BIT_MARK_US, NEC_BIT_MARK_TOLERANCE_PCT)) {
      return IR_NEC_NONE;
    }
    /* The space decides 0 or 1: anything closer to 1690 than to 560 is a 1,
     * i.e. above the midpoint (1125 us). Reject silly values. */
    uint32_t space = bit->duration1;
    if (space < 200 || space > 2500) {
      return IR_NEC_NONE;
    }
    bool one = space > (NEC_ZERO_SPACE_US + NEC_ONE_SPACE_US) / 2;

    if (one) {
      bytes[i / 8] |= (uint8_t)(1u << (i % 8)); /* LSB first within a byte */
    }
    raw_msb = (raw_msb << 1) | (one ? 1u : 0u);  /* sending order, MSB first */
  }

  /* The command byte must be followed by its inverse -- that's how we know
   * the frame arrived intact. (Some remotes skip the address check, so we
   * only use it to tell standard from extended NEC.) */
  if ((uint8_t)(bytes[2] ^ bytes[3]) != 0xFF) {
    return IR_NEC_NONE;
  }

  if ((uint8_t)(bytes[0] ^ bytes[1]) == 0xFF) {
    out->address = bytes[0];
    out->extended = false;
  } else {
    out->address = (uint16_t)(bytes[0] | (bytes[1] << 8));
    out->extended = true;
  }
  out->command = bytes[2];
  out->raw_msb = raw_msb;
  out->kind = IR_NEC_FRAME;
  return IR_NEC_FRAME;
}
