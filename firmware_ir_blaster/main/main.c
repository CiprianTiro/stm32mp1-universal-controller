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
 *   identity.c    who this blaster is: key, certificate, pairing code, and
 *                 which hub it belongs to (kept in flash)
 *   wifi.c        joins the home network, reconnects by itself
 *   ir_service.c  owns the IR hardware (ir_tx.c sends, ir_rx.c receives,
 *                 ir_nec.c knows the NEC protocol)
 *   hub_link.c    the encrypted connection to the hub (PROTOCOL.md)
 *   main.c        start-up and the serial console (this file)
 *
 * Console commands (type `help` for the full list):
 *
 *   status                     WiFi, hub connection, firmware version
 *   wifi <name> <password>     join a network (once; kept in flash)
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
#include "esp_console.h"
#include "esp_log.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "hub_link.h"
#include "identity.h"
#include "ir_nec.h"
#include "ir_service.h"
#include "ir_tx.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "version.h"
#include "wifi.h"

static const char *TAG = "ir_blaster";

/* Pins -- see the wiki page IR-Blaster-Hardware for why these two (not a
 * strapping pin, not used by the N16R8's flash/PSRAM, not USB/UART). */
#define IR_TX_GPIO GPIO_NUM_4
#define IR_RX_GPIO GPIO_NUM_5

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

static int cmd_status(int argc, char **argv)
{
  printf("IR blaster %s, firmware %s\n", identity_device_id(), FW_VERSION);
  wifi_print_status();
  if (!identity_hub_fingerprint()) {
    printf("Hub: not paired yet (the hub's wizard asks for the code: type `pairing`)\n");
  } else {
    printf("Hub: paired with %.16s..., %s\n", identity_hub_fingerprint(),
           hub_link_connected() ? "connected" : "not connected right now");
  }
  return 0;
}

static int cmd_wifi(int argc, char **argv)
{
  if (argc == 1) {
    wifi_print_status();
    return 0;
  }
  if (argc == 2 && strcmp(argv[1], "forget") == 0) {
    esp_err_t err = wifi_forget();
    printf("WiFi network forgotten: %s\n", esp_err_to_name(err));
    return err == ESP_OK ? 0 : 1;
  }
  if (argc != 2 && argc != 3) {
    printf("usage: wifi <name> <password>   join a network\n"
           "       wifi <name>              join an open network\n"
           "       wifi                     show the connection\n"
           "       wifi forget              forget the network\n"
           "A name with spaces goes in quotes: wifi \"My Network\" secret123\n");
    return 1;
  }
  esp_err_t err = wifi_set_network(argv[1], argc == 3 ? argv[2] : "");
  if (err != ESP_OK) {
    printf("could not use that network: %s (name 1-32, password up to 63 characters)\n",
           esp_err_to_name(err));
    return 1;
  }
  printf("saved, connecting to \"%s\"... (`status` shows the result)\n", argv[1]);
  return 0;
}

static int cmd_pairing(int argc, char **argv)
{
  char code[20];
  identity_pairing_code_display(code);
  printf("Pairing code: %s\n", code);
  printf("Device:       %s\n", identity_device_id());
  printf("Certificate:  %s\n", identity_fingerprint());
  printf("Hub:          %s\n", identity_hub_fingerprint() ? "paired" : "not paired");
  return 0;
}

static int cmd_unpair(int argc, char **argv)
{
  if (!identity_hub_fingerprint()) {
    printf("not paired\n");
    return 0;
  }
  esp_err_t err = identity_clear_hub();
  hub_link_pairing_changed();
  printf("hub forgotten: %s. Add the blaster in the hub again to pair.\n", esp_err_to_name(err));
  return err == ESP_OK ? 0 : 1;
}

static int cmd_factory_reset(int argc, char **argv)
{
  if (argc != 2 || strcmp(argv[1], "yes") != 0) {
    printf("Forgets the WiFi network, the hub AND this blaster's identity: it gets\n"
           "a new certificate and a NEW PAIRING CODE (a printed label becomes wrong).\n"
           "To do it, type: factory-reset yes\n");
    return 1;
  }
  printf("factory reset, restarting...\n");
  wifi_forget();
  nvs_handle_t nvs;
  if (nvs_open("identity", NVS_READWRITE, &nvs) == ESP_OK) {
    nvs_erase_all(nvs);
    nvs_commit(nvs);
    nvs_close(nvs);
  }
  vTaskDelay(pdMS_TO_TICKS(200)); /* let the message get out */
  esp_restart();
  return 0;
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
  const esp_console_cmd_t commands[] = {
    { .command = "status", .help = "WiFi, hub connection and firmware version", .func = cmd_status },
    { .command = "wifi", .help = "Join a network: wifi <name> <password>; `wifi` shows it, `wifi forget` forgets it",
      .func = cmd_wifi },
    { .command = "pairing", .help = "Show the pairing code the hub's wizard asks for", .func = cmd_pairing },
    { .command = "unpair", .help = "Forget the hub this blaster belongs to", .func = cmd_unpair },
    { .command = "factory-reset", .help = "Forget WiFi, hub and identity (new pairing code): factory-reset yes",
      .func = cmd_factory_reset },
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

  ESP_ERROR_CHECK(identity_init());
  ESP_ERROR_CHECK(ir_service_start(IR_TX_GPIO, IR_RX_GPIO));
  ir_service_add_listener(print_heard);
  ESP_ERROR_CHECK(wifi_start());
  ESP_ERROR_CHECK(hub_link_start());

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
