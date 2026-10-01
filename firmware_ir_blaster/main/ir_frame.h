/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * ir_frame: one IR message as a list of "mark" / "space" durations (#42)
 * ===========================================================================
 *
 * Every IR remote, whatever its brand or protocol, sends the same kind of
 * thing: the LED blinks at ~38 kHz for a while (a "mark"), then stays dark
 * for a while (a "space"), then blinks again, and so on. The *lengths* of
 * those marks and spaces are the message. A protocol like NEC is just a
 * convention for what lengths mean "start", "0" and "1".
 *
 * The ESP32's RMT peripheral (Remote Control Transceiver) stores exactly
 * that: a list of rmt_symbol_word_t, each holding two (level, duration)
 * pairs. We use the same format for sending and for receiving, with one
 * convention everywhere in this firmware:
 *
 *   level 1 = mark  (IR LED blinking at 38 kHz / receiver sees IR)
 *   level 0 = space (IR LED off / receiver sees nothing)
 *   durations in microseconds (the RMT channels run at 1 MHz, 1 tick = 1 us)
 *
 * so a frame the receiver captured can be sent back out unchanged ("replay").
 */

#pragma once

#include <stddef.h>
#include "hal/rmt_types.h" /* rmt_symbol_word_t */

/* Channel resolution: 1 MHz, so one RMT tick is exactly one microsecond and
 * durations can be written in us without any conversion. */
#define IR_RESOLUTION_HZ 1000000

/* Longest frame we keep, in RMT symbols (one symbol = one mark + one space).
 * NEC needs 34. Air-conditioner remotes send their whole state every time
 * and can reach ~150-300 (e.g. Daikin sends 2-3 frames back to back), so 512
 * leaves room without costing much RAM (512 * 4 bytes = 2 KB). */
#define IR_FRAME_MAX_SYMBOLS 512

/* A single RMT duration field is 15 bits wide, so at 1 MHz the longest
 * single mark or space is 32767 us (~32.8 ms). */
#define IR_MAX_DURATION_US 0x7FFF

typedef struct {
  rmt_symbol_word_t symbols[IR_FRAME_MAX_SYMBOLS];
  size_t count; /* how many entries of symbols[] are used */
} ir_frame_t;

/* Builds one symbol in our convention: a mark of mark_us, then a space of
 * space_us. A space of 0 marks the end of a frame for the RMT transmitter. */
static inline rmt_symbol_word_t ir_symbol(uint16_t mark_us, uint16_t space_us)
{
  rmt_symbol_word_t s = {
    .level0 = 1, .duration0 = mark_us,
    .level1 = 0, .duration1 = space_us,
  };
  return s;
}
