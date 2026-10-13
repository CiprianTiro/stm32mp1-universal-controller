/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * ESP32-S3 room sensor firmware (#86)
 * ===========================================================================
 *
 * Measures the room's temperature and humidity once a minute and sends
 * them to the hub. The pieces:
 *
 *   sensor.c        finds the sensor on the I2C bus and measures
 *   sensor_link.c   the sensor's requests on the hub connection: read,
 *                   "reading" (../PROTOCOL.md)
 *   main.c          start-up and the serial console (this file)
 *
 * ...and, shared with the IR blaster, in firmware_common/uc_addon/:
 * identity (key, certificate, pairing code), WiFi, WiFi setup over
 * Bluetooth, the hub connection, and the console commands status, wifi,
 * pairing, unpair, factory-reset.
 *
 * Console commands of its own:
 *
 *   read       measure now and print it
 *   i2cscan    list what answers on the I2C bus (checking the wiring)
 *
 * KEEPING IT COOL. An ESP32 with its WiFi on warms itself, its board and
 * the air around it by a few degrees -- and a sensor next to it reads that
 * warmth, not the room's. So: the CPU runs at 80 MHz (it has very little
 * to do), the WiFi sleeps between the router's beacons (uc_addon.h's
 * wifi_power_save), the PSRAM is left off (sdkconfig.defaults), and the
 * sensor sits on a cable away from the board (hardware/README.md).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "addon_console.h"
#include "esp_console.h"
#include "esp_log.h"
#include "identity.h"
#include "nvs_flash.h"
#include "provision.h"
#include "sensor.h"
#include "sensor_link.h"
#include "uc_addon.h"
#include "version.h"
#include "wifi.h"

static const char *TAG = "room_sensor";

/* The I2C pins: on the board's left header, between 3V3 (top) and GND
 * (bottom) -- see hardware/README.md. No special function on the N16R8
 * (not a strapping pin, not flash/PSRAM, not USB or the console). */
#define SENSOR_SDA_GPIO GPIO_NUM_8
#define SENSOR_SCL_GPIO GPIO_NUM_9

/* What makes this add-on a room sensor for the shared code (uc_addon.h).
 * Its own names, so the hub never mistakes it for a blaster. */
static const uc_addon_config_t ADDON = {
  .product = "Room sensor",
  .id_prefix = "ths",
  .mdns_service = "_uc-sensor",
  .prov_prefix = "PROV_THS_",
  /* 0395185a-39fd-49b5-bcfb-ee47c0a2f65b, little-endian. Randomly
   * generated for this project; the hub's template names it too. */
  .ble_service_uuid = { 0x5b, 0xf6, 0xa2, 0xc0, 0x47, 0xee, 0xfb, 0xbc,
                        0xb5, 0x49, 0xfd, 0x39, 0x5a, 0x18, 0x95, 0x03 },
  .pair_label = "uc-ths-pair-v1",
  .pair_role = "sensor",
  .fw_version = FW_VERSION,
  .protocol_version = PROTOCOL_VERSION,
  /* Modem sleep: far less power, far less heat (see the top). */
  .wifi_power_save = true,
};

/* ---------------------------------------------------------------------------
 * Console commands
 * ------------------------------------------------------------------------- */

static void print_reading(const sensor_reading_t *r)
{
  if (!r->sensor[0]) {
    printf("no sensor found -- `i2cscan` shows what answers on the bus\n");
  } else if (!r->valid) {
    printf("%s: no reading (measuring failed)\n", r->sensor);
  } else {
    printf("%s: %.2f C, %.1f %% humidity\n", r->sensor, r->temperature_c, r->humidity_pct);
  }
}

static int cmd_read(int argc, char **argv)
{
  sensor_reading_t r;
  esp_err_t err = sensor_measure_now(&r);
  print_reading(&r);
  return err == ESP_OK ? 0 : 1;
}

static int cmd_i2cscan(int argc, char **argv)
{
  printf("I2C bus on SDA=GPIO%d, SCL=GPIO%d:\n", SENSOR_SDA_GPIO, SENSOR_SCL_GPIO);
  sensor_scan();
  printf("(SHT3x: 0x44/0x45, AHT20: 0x38, HDC1080 or SHT21/HTU21/Si7021: 0x40)\n");
  return 0;
}

/* i2c <address> [w <hex bytes...>] [wait <ms>] [r <count>]
 * e.g. `i2c 0x40 w E7 r 1` reads an SHT21's user register. */
static int cmd_i2c(int argc, char **argv)
{
  uint8_t wbytes[8], out[16];
  size_t wlen = 0, rlen = 0;
  int wait_ms = 0;
  if (argc < 2) {
    printf("usage: i2c <address> [w <hex bytes...>] [wait <ms>] [r <count 1-16>]\n"
           "       e.g. i2c 0x40 w E7 r 1\n");
    return 1;
  }
  uint8_t address = (uint8_t)strtoul(argv[1], NULL, 0);
  char mode = 0;
  for (int i = 2; i < argc; i++) {
    if (strcmp(argv[i], "w") == 0 || strcmp(argv[i], "r") == 0 || strcmp(argv[i], "wait") == 0) {
      mode = argv[i][0] == 'w' && argv[i][1] == 'a' ? 't' : argv[i][0];
    } else if (mode == 'w' && wlen < sizeof(wbytes)) {
      wbytes[wlen++] = (uint8_t)strtoul(argv[i], NULL, 16);
    } else if (mode == 'r') {
      rlen = strtoul(argv[i], NULL, 0);
      rlen = rlen > sizeof(out) ? sizeof(out) : rlen;
    } else if (mode == 't') {
      wait_ms = (int)strtol(argv[i], NULL, 0);
    }
  }
  esp_err_t err = sensor_raw(address, wbytes, wlen, wait_ms, out, rlen);
  printf("%s", esp_err_to_name(err));
  for (size_t i = 0; err == ESP_OK && i < rlen; i++) {
    printf(" %02X", out[i]);
  }
  printf("\n");
  return err == ESP_OK ? 0 : 1;
}

static void register_commands(void)
{
  /* status, wifi, pairing, unpair, factory-reset: shared (uc_addon). */
  ESP_ERROR_CHECK(uc_addon_console_register());

  const esp_console_cmd_t commands[] = {
    { .command = "read", .help = "Measure now and print temperature and humidity", .func = cmd_read },
    { .command = "i2c", .help = "Development: raw I2C: i2c <address> [w <hex bytes>] [wait <ms>] [r <count>]",
      .func = cmd_i2c },
    { .command = "i2cscan", .help = "List what answers on the I2C bus (checks the sensor's wiring)",
      .func = cmd_i2cscan },
  };
  for (size_t i = 0; i < sizeof(commands) / sizeof(commands[0]); i++) {
    ESP_ERROR_CHECK(esp_console_cmd_register(&commands[i]));
  }
  ESP_ERROR_CHECK(esp_console_register_help_command());
}

/* ---------------------------------------------------------------------------
 * Start
 * ------------------------------------------------------------------------- */

void app_main(void)
{
  /* Flash storage for WiFi settings and the identity. If the NVS area is
   * full or from an incompatible version, wipe it and start fresh. */
  esp_err_t err = nvs_flash_init();
  if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
    ESP_ERROR_CHECK(nvs_flash_erase());
    err = nvs_flash_init();
  }
  ESP_ERROR_CHECK(err);

  uc_addon_init(&ADDON);
  ESP_ERROR_CHECK(identity_init());
  ESP_ERROR_CHECK(sensor_start(SENSOR_SDA_GPIO, SENSOR_SCL_GPIO));
  ESP_ERROR_CHECK(wifi_start());
  ESP_ERROR_CHECK(provision_watch_button());
  ESP_ERROR_CHECK(sensor_link_start());

  /* Interactive console on the COM port (UART0, through the board's
   * USB-serial chip), as on the blaster. */
  esp_console_repl_t *repl = NULL;
  esp_console_repl_config_t repl_config = ESP_CONSOLE_REPL_CONFIG_DEFAULT();
  repl_config.prompt = "sensor>";
  esp_console_dev_uart_config_t uart_config = ESP_CONSOLE_DEV_UART_CONFIG_DEFAULT();
  ESP_ERROR_CHECK(esp_console_new_repl_uart(&uart_config, &repl_config, &repl));

  register_commands();

  char code[20];
  identity_pairing_code_display(code);
  ESP_LOGI(TAG, "Room sensor %s ready (firmware %s). Pairing code: %s. Type 'help'.",
           identity_device_id(), FW_VERSION, code);
  ESP_ERROR_CHECK(esp_console_start_repl(repl));
}
