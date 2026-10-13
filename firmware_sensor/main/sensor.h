/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * sensor: the temperature/humidity sensor on the I2C bus (#86)
 * ===========================================================================
 *
 * Which sensor is wired up isn't configured anywhere: at start (and again
 * every minute while none answers) the bus is searched at the addresses
 * the supported sensors use, and the first one that answers is used:
 *
 *   0x44, 0x45   Sensirion SHT3x (SHT30 / SHT31 / SHT35)
 *   0x38         Aosong AHT20 (also AHT21 / AHT25)
 *   0x40         Texas Instruments HDC1080 (told apart by its ID registers;
 *                found on a board sold as "GY-SHT21")
 *   0x40         Sensirion SHT21 -- and the parts that copied it: HTU21D,
 *                Si7021 (the "GY-21" / "GY-SHT21" boards)
 *
 * A task measures every SENSOR_PERIOD_S seconds and keeps the latest
 * reading; sensor_link.c sends it to the hub. A sensor that stops
 * answering (a loose cable) counts as gone after 3 failed measurements,
 * and is searched for again.
 *
 * Why only once a minute: room air changes slowly, and every measurement
 * warms the sensor chip a tiny bit (an SHT21 by ~0.1 C if it measured all
 * the time). Once a minute that's nothing.
 */

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "driver/gpio.h"
#include "esp_err.h"

/* Seconds between two measurements. */
#define SENSOR_PERIOD_S 60

typedef struct {
  /* The sensor's short name, as the hub sees it: "sht3x", "aht20",
   * "hdc1080", "sht21"; "" if none was found. */
  const char *sensor;
  /* false: no sensor (none found yet, or it stopped answering). */
  bool valid;
  float temperature_c;
  float humidity_pct;
  /* esp_timer time of the measurement (microseconds since start). */
  int64_t at_us;
} sensor_reading_t;

/* Starts the I2C bus on these pins and the measuring task. */
esp_err_t sensor_start(gpio_num_t sda, gpio_num_t scl);

/* The latest reading. Returns a counter that goes up with every change
 * (a new measurement, the sensor found or lost), so a caller can tell
 * whether there's something new since it last looked. */
uint32_t sensor_latest(sensor_reading_t *out);

/* Measures right now (the console's `read`) and makes it the latest. */
esp_err_t sensor_measure_now(sensor_reading_t *out);

/* Development: writes `wlen` bytes to `address`, waits `delay_ms`, then
 * reads `rlen` bytes into `out` (the console's `i2c`, for trying a chip's
 * commands by hand). Either length may be 0. */
esp_err_t sensor_raw(uint8_t address, const uint8_t *wbytes, size_t wlen, int delay_ms, uint8_t *out, size_t rlen);

/* Prints every address that answers on the bus (the console's `i2cscan`,
 * for checking the wiring). */
void sensor_scan(void);
