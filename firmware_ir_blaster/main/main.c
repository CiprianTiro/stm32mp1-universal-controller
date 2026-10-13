/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * ESP32-S3 IR blaster firmware (#42)
 * ===========================================================================
 *
 * The blaster sends IR codes for the hub and learns them from original
 * remotes. The pieces, each in its own file:
 *
 *   ir_service.c  owns the IR hardware (ir_tx.c sends, ir_rx.c receives,
 *                 ir_nec.c knows the NEC protocol)
 *   ir_link.c     the blaster's requests on the hub connection: send,
 *                 learn, "heard" (PROTOCOL.md)
 *   main.c        start-up and the serial console (this file)
 *
 * ...and, shared with the hub's other add-ons (the room sensor, #86), in
 * firmware_common/uc_addon/:
 *
 *   identity.c    who this blaster is: key, certificate, pairing code, and
 *                 which hub it belongs to (kept in flash)
 *   wifi.c        joins the home network, reconnects by itself
 *   provision.c   WiFi setup over Bluetooth (no network yet / BOOT held)
 *   hub_link.c    the encrypted connection to the hub (PROTOCOL.md)
 *
 * Console commands (type `help` for the full list; the first five are
 * shared with the other add-ons, uc_addon/addon_console.c):
 *
 *   status                     WiFi, hub connection, firmware version
 *   wifi <name> <password>     join a network (development; normally the
 *                              hub sets it over Bluetooth)
 *   wifi forget                forget it, restart into Bluetooth setup
 *   pairing                    show the pairing code the hub asks for
 *   unpair                     forget the hub
 *   nec <address> <command> [repeats]
 *                              send an NEC code, e.g. `nec 0x04 0x08`
 *   replay / last              send / print the last code heard
 *   duty, ledtest              hardware checks
 *   factory-reset yes          forget everything, new identity
 *
 * Every frame the receiver hears is printed, decoded if it's NEC.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "addon_console.h"
#include "esp_console.h"
#include "esp_log.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "identity.h"
#include "ir_link.h"
#include "ir_nec.h"
#include "ir_service.h"
#include "ir_tx.h"
#include "provision.h"
#include "nvs_flash.h"
#include "uc_addon.h"
#include "version.h"
#include "wifi.h"

static const char *TAG = "ir_blaster";

/* Pins -- see the wiki page IR-Blaster-Hardware for why these two (not a
 * strapping pin, not used by the N16R8's flash/PSRAM, not USB/UART). */
#define IR_TX_GPIO GPIO_NUM_4
#define IR_RX_GPIO GPIO_NUM_5

/* What makes this add-on an IR blaster for the shared code (uc_addon.h).
 * These names are the ones the blaster had before that code was shared
 * (#86): a blaster paired before keeps working with its hub. */
static const uc_addon_config_t ADDON = {
  .product = "IR blaster",
  .id_prefix = "irb",
  .mdns_service = "_uc-irblaster",
  .prov_prefix = "PROV_IRB_",
  /* 1f2b9c64-3a5e-4c1d-9f0a-5b6e7d8c9a01, little-endian. Randomly
   * generated for this project; the hub's template names it too. */
  .ble_service_uuid = { 0x01, 0x9a, 0x8c, 0x7d, 0x6e, 0x5b, 0x0a, 0x9f,
                        0x1d, 0x4c, 0x5e, 0x3a, 0x64, 0x9c, 0x2b, 0x1f },
  .pair_label = "uc-irb-pair-v1",
  .pair_role = "blaster",
  .fw_version = FW_VERSION,
  .protocol_version = PROTOCOL_VERSION,
  /* IR must go out the moment the hub says: no WiFi power saving. */
  .wifi_power_save = false,
};

/* Frame buffer for the console commands. Static: 2 KB. Only the console
 * task uses it. */
static ir_frame_t console_frame;

/* ---------------------------------------------------------------------------
 * Printing frames
 * ------------------------------------------------------------------------- */

/* Prints a frame as "+mark -space +mark -space ..." in microseconds, 8
 * symbols per line. This is the raw form that works for any protocol. */
static void print_raw(const ir_frame_t *frame)
{
  for (size_t i = 0; i < frame->count; i++) {
    const rmt_symbol_word_t *s = &frame->symbols[i];
    printf(" +%u", s->duration0);
    if (s->duration1) {
      printf(" -%u", s->duration1);
    }
    if (i % 8 == 7 || i == frame->count - 1) {
      printf("\n");
    }
  }
}

/* Prints what a frame is: NEC (with address/command) or unknown + raw. */
static void describe(const ir_frame_t *frame)
{
  ir_nec_result_t nec;

  switch (ir_nec_decode(frame, &nec)) {
  case IR_NEC_FRAME:
    printf("NEC%s address=0x%02X command=0x%02X (0x%08lX)\n",
           nec.extended ? " (extended)" : "", nec.address, nec.command,
           (unsigned long)nec.raw_msb);
    break;
  case IR_NEC_REPEAT:
    printf("NEC repeat (button held)\n");
    break;
  default:
    printf("unknown protocol, %u marks, durations in us:\n", (unsigned)frame->count);
    print_raw(frame);
    break;
  }
}

/* ir_service listener: print every frame heard. */
static void print_heard(const ir_frame_t *frame, bool echo)
{
  printf("\nIR received%s: ", echo ? " (our own send)" : "");
  describe(frame);
}

/* ---------------------------------------------------------------------------
 * Console commands. Each gets argc/argv like a small program's main().
 * Returning non-zero makes the console print an error.
 * ------------------------------------------------------------------------- */

/* Parses a number, decimal or 0x-hex. Returns false if it isn't one or is
 * above `max`. */
static bool parse_number(const char *text, unsigned long max, unsigned long *out)
{
  char *end;
  unsigned long value = strtoul(text, &end, 0);
  if (end == text || *end != '\0' || value > max) {
    return false;
  }
  *out = value;
  return true;
}

static int cmd_nec(int argc, char **argv)
{
  unsigned long address, command, repeats = 0;

  if (argc < 3 || argc > 4 || !parse_number(argv[1], 0xFFFF, &address) ||
      !parse_number(argv[2], 0xFF, &command) ||
      (argc == 4 && !parse_number(argv[3], 20, &repeats))) {
    printf("usage: nec <address 0-0xFFFF> <command 0-0xFF> [repeats 0-20]\n"
           "       e.g. nec 0x04 0x08, or nec 0x00 0x45 3 (as if held briefly)\n");
    return 1;
  }

  esp_err_t err = ir_service_send_nec((uint16_t)address, (uint8_t)command, (int)repeats);
  printf("sent NEC address=0x%02lX command=0x%02lX + %lu repeats: %s\n", address, command,
         repeats, esp_err_to_name(err));
  return err == ESP_OK ? 0 : 1;
}

static int cmd_replay(int argc, char **argv)
{
  if (!ir_service_last(&console_frame)) {
    printf("nothing heard yet -- point a remote at the receiver and press a button\n");
    return 1;
  }
  esp_err_t err = ir_service_send(&console_frame, 0);
  printf("replayed %u marks: %s\n", (unsigned)console_frame.count, esp_err_to_name(err));
  return err == ESP_OK ? 0 : 1;
}

static int cmd_last(int argc, char **argv)
{
  if (!ir_service_last(&console_frame)) {
    printf("nothing heard yet\n");
    return 1;
  }
  describe(&console_frame);
  print_raw(&console_frame);
  return 0;
}

static int cmd_duty(int argc, char **argv)
{
  unsigned long percent;

  if (argc != 2 || !parse_number(argv[1], 50, &percent) || percent < 10) {
    printf("usage: duty <percent 10-50>, e.g. duty 50 (default after start: 33)\n");
    return 1;
  }
  esp_err_t err = ir_tx_set_duty((int)percent);
  printf("carrier duty %lu %%: %s\n", percent, esp_err_to_name(err));
  return err == ESP_OK ? 0 : 1;
}

static int cmd_ledtest(int argc, char **argv)
{
  /* Short on purpose: with the 2-LED circuit ~65 mA flows steadily during
   * the test, fine for seconds but not for minutes (no LED datasheet). */
  unsigned long seconds = 3;

  if (argc > 2 || (argc == 2 && (!parse_number(argv[1], 5, &seconds) || seconds == 0))) {
    printf("usage: ledtest [seconds 1-5], default 3\n");
    return 1;
  }

  printf("IR LEDs steadily ON for %lu s (no carrier). Measure LED 1's long leg\n"
         "against GND now: ~2.9 V = LED circuit OK (4.4-4.6 V = no current).\n", seconds);
  esp_err_t err = ir_service_led_test((int)seconds);
  printf("IR LED off: %s\n", esp_err_to_name(err));
  return err == ESP_OK ? 0 : 1;
}

static void register_commands(void)
{
  /* status, wifi, pairing, unpair, factory-reset: shared (uc_addon). */
  ESP_ERROR_CHECK(uc_addon_console_register());

  const esp_console_cmd_t commands[] = {
    { .command = "nec", .help = "Send an NEC code: nec <address> <command> [repeats] (LG TV power: nec 0x04 0x08)",
      .func = cmd_nec },
    { .command = "replay", .help = "Send the last code the receiver heard", .func = cmd_replay },
    { .command = "last", .help = "Print the last code the receiver heard", .func = cmd_last },
    { .command = "duty", .help = "Set the 38 kHz carrier's duty cycle: duty <10-50> (percent, default 33)",
      .func = cmd_duty },
    { .command = "ledtest", .help = "Hardware check: IR LEDs steadily on: ledtest [seconds 1-5], default 3",
      .func = cmd_ledtest },
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
  ESP_ERROR_CHECK(ir_service_start(IR_TX_GPIO, IR_RX_GPIO));
  ir_service_add_listener(print_heard);
  ESP_ERROR_CHECK(wifi_start());
  ESP_ERROR_CHECK(provision_watch_button());
  ESP_ERROR_CHECK(ir_link_start());

  /* Interactive console on the COM port (UART0, through the board's
   * USB-serial chip) -- see sdkconfig.defaults for why COM, not USB. */
  esp_console_repl_t *repl = NULL;
  esp_console_repl_config_t repl_config = ESP_CONSOLE_REPL_CONFIG_DEFAULT();
  repl_config.prompt = "ir>";
  esp_console_dev_uart_config_t uart_config = ESP_CONSOLE_DEV_UART_CONFIG_DEFAULT();
  ESP_ERROR_CHECK(esp_console_new_repl_uart(&uart_config, &repl_config, &repl));

  register_commands();

  char code[20];
  identity_pairing_code_display(code);
  ESP_LOGI(TAG, "IR blaster %s ready (firmware %s). Pairing code: %s. Type 'help'.",
           identity_device_id(), FW_VERSION, code);
  ESP_ERROR_CHECK(esp_console_start_repl(repl));
}
