/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * The console commands every add-on has -- see console.h. (Moved here
 * from firmware_ir_blaster/main/main.c for #86.)
 */

#include "addon_console.h"

#include <stdio.h>
#include <string.h>
#include "esp_console.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "hub_link.h"
#include "identity.h"
#include "nvs.h"
#include "provision.h"
#include "uc_addon.h"
#include "wifi.h"

/* ---------------------------------------------------------------------------
 * Each command gets argc/argv like a small program's main(). Returning
 * non-zero makes the console print an error.
 * ------------------------------------------------------------------------- */

static int cmd_status(int argc, char **argv)
{
  printf("%s %s, firmware %s\n", uc_addon_config()->product, identity_device_id(), uc_addon_config()->fw_version);
  if (provision_active()) {
    printf("WiFi: not set up. Bluetooth setup is waiting (the hub's wizard, pairing code: `pairing`)\n");
  } else {
    wifi_print_status();
  }
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
    /* Like holding BOOT: Bluetooth setup needs a restart (its memory was
     * released once the WiFi was set up). */
    esp_err_t err = wifi_forget();
    printf("WiFi network forgotten (%s), restarting into Bluetooth setup...\n", esp_err_to_name(err));
    vTaskDelay(pdMS_TO_TICKS(200));
    esp_restart();
    return 0;
  }
  if (argc != 2 && argc != 3) {
    printf("usage: wifi <name> <password>   join a network\n"
           "       wifi <name>              join an open network\n"
           "       wifi                     show the connection\n"
           "       wifi forget              forget the network, restart into Bluetooth setup\n"
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
  printf("hub forgotten: %s. Add the %s in the hub again to pair.\n", esp_err_to_name(err),
         uc_addon_config()->product);
  return err == ESP_OK ? 0 : 1;
}

static int cmd_factory_reset(int argc, char **argv)
{
  if (argc != 2 || strcmp(argv[1], "yes") != 0) {
    printf("Forgets the WiFi network, the hub AND this add-on's identity: it gets\n"
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

esp_err_t uc_addon_console_register(void)
{
  const esp_console_cmd_t commands[] = {
    { .command = "status", .help = "WiFi, hub connection and firmware version", .func = cmd_status },
    { .command = "wifi", .help = "Join a network: wifi <name> <password>; `wifi` shows it, `wifi forget` forgets it",
      .func = cmd_wifi },
    { .command = "pairing", .help = "Show the pairing code the hub's wizard asks for", .func = cmd_pairing },
    { .command = "unpair", .help = "Forget the hub this add-on belongs to", .func = cmd_unpair },
    { .command = "factory-reset", .help = "Forget WiFi, hub and identity (new pairing code): factory-reset yes",
      .func = cmd_factory_reset },
  };

  for (size_t i = 0; i < sizeof(commands) / sizeof(commands[0]); i++) {
    esp_err_t err = esp_console_cmd_register(&commands[i]);
    if (err != ESP_OK) {
      return err;
    }
  }
  return ESP_OK;
}
