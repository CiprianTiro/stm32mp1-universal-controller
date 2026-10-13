/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * Sensor -- see sensor.h
 * ===========================================================================
 *
 * All four sensor families work the same way from here:
 *
 *   1. write a "measure" command
 *   2. wait until the measurement is done (a fixed time from the datasheet,
 *      a little more to be safe)
 *   3. read the result: raw 16-bit (or 20-bit) numbers, each followed by a
 *      CRC byte that proves it arrived unchanged
 *   4. turn the raw numbers into C and % with the datasheet's formula
 *
 * Step 2 waits instead of letting the sensor hold the bus ("clock
 * stretching"): the "no hold" commands are the ones every copy of these
 * chips does the same way.
 *
 * The I2C bus is used by the measuring task and by the console (`read`,
 * `i2cscan`): a mutex makes sure one at a time.
 */

#include "sensor.h"

#include <stdio.h>
#include <string.h>
#include "driver/i2c_master.h"
#include "esp_check.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *TAG = "sensor";

/* 100 kHz: "standard mode", which every one of these chips supports, and
 * forgiving of a long cable (the sensor sits away from the ESP32). */
#define I2C_HZ 100000

/* One I2C transfer may take this long before it counts as failed. */
#define I2C_TIMEOUT_MS 50

/* Failed measurements in a row before the sensor counts as gone. */
#define MAX_FAILS 3

/* ---------------------------------------------------------------------------
 * The sensor families
 * ------------------------------------------------------------------------- */

typedef struct sensor_type sensor_type_t;
struct sensor_type {
  const char *name;
  uint8_t address;
  /* Is the chip at `address` really this one? Several families share
   * 0x40. NULL: whatever answers there is taken. */
  bool (*is)(i2c_master_dev_handle_t dev);
  /* Gets the chip ready once it's found (a reset, a calibration). */
  esp_err_t (*init)(i2c_master_dev_handle_t dev);
  esp_err_t (*measure)(i2c_master_dev_handle_t dev, float *temperature_c, float *humidity_pct);
};

static i2c_master_bus_handle_t bus;
static i2c_master_dev_handle_t dev;   /* the sensor found, or NULL */
static const sensor_type_t *type;     /* its family, or NULL */
static SemaphoreHandle_t lock;        /* the bus, and everything below */
static sensor_reading_t latest = { .sensor = "" };
static uint32_t changes;
static int fails;

/* CRC-8 as Sensirion and Aosong use it: polynomial x^8 + x^5 + x^4 + 1
 * (0x31), starting from `init` (0xFF for SHT3x / AHT20, 0x00 for SHT21).
 * The sensor sends one after the data; computing it again here and
 * comparing tells whether a bit flipped on the way (a long cable). */
static uint8_t crc8(const uint8_t *data, size_t len, uint8_t init)
{
  uint8_t crc = init;
  for (size_t i = 0; i < len; i++) {
    crc ^= data[i];
    for (int bit = 0; bit < 8; bit++) {
      crc = (crc & 0x80) ? (uint8_t)((crc << 1) ^ 0x31) : (uint8_t)(crc << 1);
    }
  }
  return crc;
}

/* Waits AT LEAST `ms`. vTaskDelay counts in ticks of 10 ms, and the
 * current tick is already partly over: pdMS_TO_TICKS(20) = 2 ticks may end
 * after only ~11 ms -- before a 13 ms measurement is done (the chip then
 * refuses the read). One tick more makes it never shorter. */
static void wait_ms(int ms)
{
  vTaskDelay(pdMS_TO_TICKS(ms) + 1);
}

static esp_err_t write_bytes(i2c_master_dev_handle_t d, const uint8_t *bytes, size_t len)
{
  return i2c_master_transmit(d, bytes, len, I2C_TIMEOUT_MS);
}

static esp_err_t read_bytes(i2c_master_dev_handle_t d, uint8_t *bytes, size_t len)
{
  return i2c_master_receive(d, bytes, len, I2C_TIMEOUT_MS);
}

/* ---- Sensirion SHT3x (datasheet: "SHT3x-DIS") ---- */

static esp_err_t sht3x_init(i2c_master_dev_handle_t d)
{
  /* Soft reset (0x30A2); the chip is ready again after 1.5 ms. */
  const uint8_t reset[] = { 0x30, 0xA2 };
  esp_err_t err = write_bytes(d, reset, sizeof(reset));
  wait_ms(5);
  return err;
}

static esp_err_t sht3x_measure(i2c_master_dev_handle_t d, float *t, float *rh)
{
  /* One measurement, high repeatability (the most precise), no clock
   * stretching: 0x2400. Takes up to 15.5 ms. */
  const uint8_t measure[] = { 0x24, 0x00 };
  ESP_RETURN_ON_ERROR(write_bytes(d, measure, sizeof(measure)), TAG, "sht3x start");
  wait_ms(20);

  /* temperature MSB, LSB, CRC, humidity MSB, LSB, CRC */
  uint8_t b[6];
  ESP_RETURN_ON_ERROR(read_bytes(d, b, sizeof(b)), TAG, "sht3x read");
  if (crc8(&b[0], 2, 0xFF) != b[2] || crc8(&b[3], 2, 0xFF) != b[5]) {
    return ESP_ERR_INVALID_CRC;
  }
  uint16_t raw_t = (uint16_t)(b[0] << 8 | b[1]);
  uint16_t raw_rh = (uint16_t)(b[3] << 8 | b[4]);
  *t = -45.0f + 175.0f * raw_t / 65535.0f;
  *rh = 100.0f * raw_rh / 65535.0f;
  return ESP_OK;
}

/* ---- Aosong AHT20 (datasheet: "AHT20 Product manuals") ---- */

static esp_err_t aht20_init(i2c_master_dev_handle_t d)
{
  /* The status byte's bit 3 says "calibrated". A chip that isn't (fresh
   * from power-on, on some batches) gets the init command 0xBE 08 00. */
  uint8_t status = 0;
  ESP_RETURN_ON_ERROR(read_bytes(d, &status, 1), TAG, "aht20 status");
  if (!(status & 0x08)) {
    const uint8_t init[] = { 0xBE, 0x08, 0x00 };
    ESP_RETURN_ON_ERROR(write_bytes(d, init, sizeof(init)), TAG, "aht20 init");
    wait_ms(10);
  }
  return ESP_OK;
}

static esp_err_t aht20_measure(i2c_master_dev_handle_t d, float *t, float *rh)
{
  /* "Trigger measurement": 0xAC 33 00. Takes about 80 ms. */
  const uint8_t measure[] = { 0xAC, 0x33, 0x00 };
  ESP_RETURN_ON_ERROR(write_bytes(d, measure, sizeof(measure)), TAG, "aht20 start");
  wait_ms(85);

  /* status, then 20 bits of humidity and 20 bits of temperature packed
   * into 5 bytes, then a CRC. Status bit 7 = still busy: wait a bit. */
  uint8_t b[7];
  for (int tries = 0;; tries++) {
    ESP_RETURN_ON_ERROR(read_bytes(d, b, sizeof(b)), TAG, "aht20 read");
    if (!(b[0] & 0x80)) {
      break;
    }
    if (tries == 3) {
      return ESP_ERR_TIMEOUT;
    }
    wait_ms(20);
  }
  if (crc8(b, 6, 0xFF) != b[6]) {
    return ESP_ERR_INVALID_CRC;
  }
  uint32_t raw_rh = (uint32_t)b[1] << 12 | (uint32_t)b[2] << 4 | b[3] >> 4;
  uint32_t raw_t = (uint32_t)(b[3] & 0x0F) << 16 | (uint32_t)b[4] << 8 | b[5];
  *rh = raw_rh * 100.0f / 1048576.0f;
  *t = raw_t * 200.0f / 1048576.0f - 50.0f;
  return ESP_OK;
}

/* ---- Sensirion SHT21, HTU21D, Si7021 (datasheet: "SHT21") ---- */

static esp_err_t sht21_init(i2c_master_dev_handle_t d)
{
  /* Soft reset (0xFE); ready again after 15 ms. */
  const uint8_t reset = 0xFE;
  esp_err_t err = write_bytes(d, &reset, 1);
  wait_ms(20);
  return err;
}

/* One SHT21 measurement: `command` 0xF3 (temperature) or 0xF5 (humidity),
 * "no hold master" mode. Returns the raw value with its 2 status bits
 * cleared. Bit 1 of those says what was measured (1 = humidity): checked,
 * so a mixed-up answer is never taken for the other quantity. */
static esp_err_t sht21_raw(i2c_master_dev_handle_t d, uint8_t command, int delay_ms, uint16_t *raw)
{
  ESP_RETURN_ON_ERROR(write_bytes(d, &command, 1), TAG, "sht21 start");
  wait_ms(delay_ms);
  uint8_t b[3];
  ESP_RETURN_ON_ERROR(read_bytes(d, b, sizeof(b)), TAG, "sht21 read");
  if (crc8(b, 2, 0x00) != b[2]) {
    return ESP_ERR_INVALID_CRC;
  }
  bool is_humidity = b[1] & 0x02;
  if (is_humidity != (command == 0xF5)) {
    return ESP_ERR_INVALID_RESPONSE;
  }
  *raw = (uint16_t)(b[0] << 8 | b[1]) & ~0x0003;
  return ESP_OK;
}

static esp_err_t sht21_measure(i2c_master_dev_handle_t d, float *t, float *rh)
{
  /* Temperature at 14 bits takes up to 85 ms, humidity at 12 bits 29 ms
   * (the chip's default resolutions). */
  uint16_t raw_t, raw_rh;
  ESP_RETURN_ON_ERROR(sht21_raw(d, 0xF3, 90, &raw_t), TAG, "temperature");
  ESP_RETURN_ON_ERROR(sht21_raw(d, 0xF5, 35, &raw_rh), TAG, "humidity");
  *t = -46.85f + 175.72f * raw_t / 65536.0f;
  *rh = -6.0f + 125.0f * raw_rh / 65536.0f;
  return ESP_OK;
}

/* ---- Texas Instruments HDC1080 (datasheet: "HDC1080") ----
 *
 * Also at 0x40, but a different kind of chip: registers instead of
 * commands. Found on boards sold as "GY-SHT21" (#86: the one on the bench
 * was one). Told apart by its ID registers: 0xFE = 0x5449 ("TI"), 0xFF =
 * 0x1050. (An SHT21 takes the 0xFE write as a reset and doesn't answer the
 * read, so asking is harmless.) */

static bool hdc1080_is(i2c_master_dev_handle_t d)
{
  uint8_t pointer = 0xFE, id[2];
  if (write_bytes(d, &pointer, 1) != ESP_OK || read_bytes(d, id, 2) != ESP_OK) {
    return false;
  }
  if (id[0] != 0x54 || id[1] != 0x49) {
    return false;
  }
  pointer = 0xFF;
  return write_bytes(d, &pointer, 1) == ESP_OK && read_bytes(d, id, 2) == ESP_OK && id[0] == 0x10 && id[1] == 0x50;
}

static esp_err_t hdc1080_init(i2c_master_dev_handle_t d)
{
  /* Configuration register (0x02) = 0x1000: measure temperature AND
   * humidity in one go, both at 14 bits; heater off. */
  const uint8_t config[] = { 0x02, 0x10, 0x00 };
  ESP_RETURN_ON_ERROR(write_bytes(d, config, sizeof(config)), TAG, "hdc1080 config");
  wait_ms(15);
  return ESP_OK;
}

static esp_err_t hdc1080_measure(i2c_master_dev_handle_t d, float *t, float *rh)
{
  /* Pointing at the temperature register (0x00) starts a measurement;
   * both take 6.5 ms each at 14 bits. Then 4 bytes: temperature MSB, LSB,
   * humidity MSB, LSB. No CRC on this chip. */
  const uint8_t pointer = 0x00;
  ESP_RETURN_ON_ERROR(write_bytes(d, &pointer, 1), TAG, "hdc1080 start");
  wait_ms(20);
  uint8_t b[4];
  ESP_RETURN_ON_ERROR(read_bytes(d, b, sizeof(b)), TAG, "hdc1080 read");
  uint16_t raw_t = (uint16_t)(b[0] << 8 | b[1]);
  uint16_t raw_rh = (uint16_t)(b[2] << 8 | b[3]);
  *t = raw_t * 165.0f / 65536.0f - 40.0f;
  *rh = raw_rh * 100.0f / 65536.0f;
  return ESP_OK;
}

/* In the order they're looked for. The HDC1080 before the SHT21: both use
 * 0x40, and only the HDC1080 can be asked who it is. */
static const sensor_type_t TYPES[] = {
  { "sht3x", 0x44, NULL, sht3x_init, sht3x_measure },
  { "sht3x", 0x45, NULL, sht3x_init, sht3x_measure },
  { "aht20", 0x38, NULL, aht20_init, aht20_measure },
  { "hdc1080", 0x40, hdc1080_is, hdc1080_init, hdc1080_measure },
  { "sht21", 0x40, NULL, sht21_init, sht21_measure },
};

/* ---------------------------------------------------------------------------
 * Finding the sensor, measuring (with `lock` held)
 * ------------------------------------------------------------------------- */

/* Looks for a sensor at each known address. Returns true if one was found
 * (then `dev` and `type` are set). */
static bool find(void)
{
  for (size_t i = 0; i < sizeof(TYPES) / sizeof(TYPES[0]); i++) {
    if (i2c_master_probe(bus, TYPES[i].address, I2C_TIMEOUT_MS) != ESP_OK) {
      continue;
    }
    i2c_device_config_t config = {
      .dev_addr_length = I2C_ADDR_BIT_LEN_7,
      .device_address = TYPES[i].address,
      .scl_speed_hz = I2C_HZ,
    };
    if (i2c_master_bus_add_device(bus, &config, &dev) != ESP_OK) {
      continue;
    }
    if ((TYPES[i].is && !TYPES[i].is(dev)) || TYPES[i].init(dev) != ESP_OK) {
      i2c_master_bus_rm_device(dev);
      dev = NULL;
      continue;
    }
    type = &TYPES[i];
    fails = 0;
    ESP_LOGI(TAG, "found %s at 0x%02x", type->name, type->address);
    return true;
  }
  return false;
}

static void forget(void)
{
  if (dev) {
    i2c_master_bus_rm_device(dev);
  }
  dev = NULL;
  type = NULL;
}

/* One measurement into `latest`. Finds the sensor first if needed. */
static esp_err_t measure(void)
{
  bool was_valid = latest.valid;

  if (!type && !find()) {
    if (was_valid || latest.sensor[0]) {
      latest = (sensor_reading_t){ .sensor = "" };
      changes++;
    }
    return ESP_ERR_NOT_FOUND;
  }

  float t, rh;
  esp_err_t err = type->measure(dev, &t, &rh);
  if (err != ESP_OK) {
    ESP_LOGW(TAG, "%s: measuring failed: %s", type->name, esp_err_to_name(err));
    if (++fails >= MAX_FAILS) {
      ESP_LOGW(TAG, "%s stopped answering, looking for a sensor again", type->name);
      forget();
      latest = (sensor_reading_t){ .sensor = "" };
      changes++;
    }
    return err;
  }
  fails = 0;

  /* The formulas can go a little past 0-100 % at the extremes. */
  if (rh < 0) {
    rh = 0;
  } else if (rh > 100) {
    rh = 100;
  }
  latest = (sensor_reading_t){
    .sensor = type->name,
    .valid = true,
    .temperature_c = t,
    .humidity_pct = rh,
    .at_us = esp_timer_get_time(),
  };
  changes++;
  return ESP_OK;
}

static void sensor_task(void *arg)
{
  for (;;) {
    xSemaphoreTake(lock, portMAX_DELAY);
    measure();
    xSemaphoreGive(lock);
    vTaskDelay(pdMS_TO_TICKS(SENSOR_PERIOD_S * 1000));
  }
}

/* ---------------------------------------------------------------------------
 * The public functions
 * ------------------------------------------------------------------------- */

esp_err_t sensor_start(gpio_num_t sda, gpio_num_t scl)
{
  lock = xSemaphoreCreateMutex();
  ESP_RETURN_ON_FALSE(lock, ESP_ERR_NO_MEM, TAG, "mutex");

  i2c_master_bus_config_t config = {
    .i2c_port = -1, /* any free I2C controller */
    .sda_io_num = sda,
    .scl_io_num = scl,
    .clk_source = I2C_CLK_SRC_DEFAULT,
    .glitch_ignore_cnt = 7,
    /* The breakout boards have their own pull-up resistors (the bus needs
     * them); the chip's weak internal ones are only a fallback for a bare
     * sensor on a short cable. */
    .flags.enable_internal_pullup = true,
  };
  ESP_RETURN_ON_ERROR(i2c_new_master_bus(&config, &bus), TAG, "I2C bus");

  BaseType_t ok = xTaskCreate(sensor_task, "sensor", 4096, NULL, 3, NULL);
  ESP_RETURN_ON_FALSE(ok == pdPASS, ESP_ERR_NO_MEM, TAG, "task");
  return ESP_OK;
}

uint32_t sensor_latest(sensor_reading_t *out)
{
  xSemaphoreTake(lock, portMAX_DELAY);
  *out = latest;
  uint32_t n = changes;
  xSemaphoreGive(lock);
  return n;
}

esp_err_t sensor_measure_now(sensor_reading_t *out)
{
  xSemaphoreTake(lock, portMAX_DELAY);
  esp_err_t err = measure();
  *out = latest;
  xSemaphoreGive(lock);
  return err;
}

esp_err_t sensor_raw(uint8_t address, const uint8_t *wbytes, size_t wlen, int delay_ms, uint8_t *out, size_t rlen)
{
  i2c_device_config_t config = {
    .dev_addr_length = I2C_ADDR_BIT_LEN_7,
    .device_address = address,
    .scl_speed_hz = I2C_HZ,
  };
  i2c_master_dev_handle_t d;
  xSemaphoreTake(lock, portMAX_DELAY);
  esp_err_t err = i2c_master_bus_add_device(bus, &config, &d);
  if (err == ESP_OK) {
    if (wlen) {
      err = write_bytes(d, wbytes, wlen);
    }
    if (err == ESP_OK && delay_ms) {
      wait_ms(delay_ms);
    }
    if (err == ESP_OK && rlen) {
      err = read_bytes(d, out, rlen);
    }
    i2c_master_bus_rm_device(d);
  }
  xSemaphoreGive(lock);
  return err;
}

void sensor_scan(void)
{
  int found = 0;
  xSemaphoreTake(lock, portMAX_DELAY);
  for (uint16_t address = 0x08; address < 0x78; address++) {
    if (i2c_master_probe(bus, address, I2C_TIMEOUT_MS) == ESP_OK) {
      printf("  0x%02x answers\n", address);
      found++;
    }
  }
  xSemaphoreGive(lock);
  if (!found) {
    printf("  nothing answers: check SDA, SCL, 3V3 and GND (hardware/README.md)\n");
  }
}
