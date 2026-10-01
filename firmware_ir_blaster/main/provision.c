/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * WiFi setup over Bluetooth -- see provision.h.
 *
 * The provisioning manager (Espressif's network_provisioning component)
 * does the Bluetooth GATT service, the SRP6a session and the WiFi
 * configuration endpoints. We give it the security parameters, react to
 * its events, and decide when it runs.
 */

#include "provision.h"

#include <stdio.h>
#include <string.h>
#include "driver/gpio.h"
#include "esp_check.h"
#include "esp_log.h"
#include "esp_system.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "identity.h"
#include "network_provisioning/manager.h"
#include "network_provisioning/scheme_ble.h"
#include "esp_srp.h"
#include "wifi.h"

static const char *TAG = "provision";

/* SRP6a user name: Espressif's default, which their esp_prov tool and
 * phone app use unless told otherwise. It's not a secret -- the pairing
 * code (the SRP password) is. */
#define SEC2_USERNAME "wifiprov"

/* The salt is random for each setup; 16 bytes is the example's size. */
#define SEC2_SALT_LEN 16

/* Our own GATT service UUID, advertised so the hub finds IR blasters among
 * all the Bluetooth devices around (instead of every Espressif device).
 * Randomly generated for this project; the byte order is little-endian,
 * as the component expects: 1f2b9c64-3a5e-4c1d-9f0a-5b6e7d8c9a01. */
static uint8_t service_uuid[16] = {
  0x01, 0x9a, 0x8c, 0x7d, 0x6e, 0x5b, 0x0a, 0x9f,
  0x1d, 0x4c, 0x5e, 0x3a, 0x64, 0x9c, 0x2b, 0x1f,
};

/* How often the WiFi join is tried with the received network before the
 * setup client is told "failed" (so the person can correct a password). */
#define WIFI_ATTEMPTS 4

/* BOOT button (GPIO0): held this long = restart into Bluetooth setup. */
#define BOOT_GPIO GPIO_NUM_0
#define HOLD_MS 5000

static volatile bool active;

/* The SRP6a salt and verifier, made from the pairing code at start. They
 * must stay valid until the manager says NETWORK_PROV_END. */
static char *salt;
static char *verifier;
static int verifier_len;
static network_prov_security2_params_t sec2_params;

static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
  switch (id) {
  case NETWORK_PROV_START:
    ESP_LOGI(TAG, "Bluetooth setup running: the hub's wizard (or esp_prov) can set the WiFi now");
    break;
  case NETWORK_PROV_WIFI_CRED_RECV: {
    /* Never print the password (unlike Espressif's example). */
    wifi_sta_config_t *sta = data;
    ESP_LOGI(TAG, "received WiFi network \"%.*s\", joining...", (int)sizeof(sta->ssid), (char *)sta->ssid);
    break;
  }
  case NETWORK_PROV_WIFI_CRED_FAIL: {
    network_prov_wifi_sta_fail_reason_t *reason = data;
    ESP_LOGW(TAG, "could not join: %s. Waiting for corrected details.",
             *reason == NETWORK_PROV_WIFI_STA_AUTH_ERROR ? "wrong WiFi password" : "network not found");
    /* Let the client send corrected details in the same session. */
    network_prov_mgr_reset_wifi_sm_state_on_failure();
    break;
  }
  case NETWORK_PROV_WIFI_CRED_SUCCESS:
    ESP_LOGI(TAG, "joined the WiFi");
    break;
  case NETWORK_PROV_END:
    /* The manager stops Bluetooth by itself shortly after success. */
    network_prov_mgr_deinit();
    active = false;
    wifi_no_power_save(); /* allowed now that Bluetooth is off */
    free(salt);
    free(verifier);
    salt = verifier = NULL;
    ESP_LOGI(TAG, "Bluetooth setup finished, Bluetooth off");
    break;
  default:
    break;
  }
}

esp_err_t provision_start_if_needed(bool *started)
{
  *started = false;

  network_prov_mgr_config_t config = {
    .scheme = network_prov_scheme_ble,
    /* Free the Bluetooth stack's memory once it's not needed (right away
     * if the WiFi is already set up). Setting up again later restarts the
     * chip (BOOT held), so nothing needs it back. */
    .scheme_event_handler = NETWORK_PROV_SCHEME_BLE_EVENT_HANDLER_FREE_BTDM,
    .network_prov_wifi_conn_cfg = { .wifi_conn_attempts = WIFI_ATTEMPTS },
  };
  ESP_RETURN_ON_ERROR(network_prov_mgr_init(config), TAG, "manager init");

  bool provisioned = false;
  ESP_RETURN_ON_ERROR(network_prov_mgr_is_wifi_provisioned(&provisioned), TAG, "state");
  if (provisioned) {
    network_prov_mgr_deinit(); /* also frees the Bluetooth memory */
    return ESP_OK;
  }

  ESP_RETURN_ON_ERROR(esp_event_handler_register(NETWORK_PROV_EVENT, ESP_EVENT_ANY_ID, on_event, NULL),
                      TAG, "events");

  /* Security 2 parameters from the pairing code. Generating the verifier
   * takes about a second on the ESP32-S3 (big-number maths). */
  const char *code = identity_pairing_code();
  ESP_RETURN_ON_ERROR(esp_srp_gen_salt_verifier(SEC2_USERNAME, strlen(SEC2_USERNAME), code, strlen(code),
                                                &salt, SEC2_SALT_LEN, &verifier, &verifier_len),
                      TAG, "SRP verifier");
  sec2_params.salt = salt;
  sec2_params.salt_len = SEC2_SALT_LEN;
  sec2_params.verifier = verifier;
  sec2_params.verifier_len = verifier_len;

  /* "PROV_IRB_91E4": Espressif's apps list names starting with PROV_; the
   * last 4 digits of the device id tell two blasters apart. */
  char name[20];
  const char *id = identity_device_id();
  snprintf(name, sizeof(name), "PROV_IRB_%s", id + strlen(id) - 4);
  for (char *c = name; *c; c++) {
    if (*c >= 'a' && *c <= 'f') {
      *c -= 'a' - 'A';
    }
  }

  network_prov_scheme_ble_set_service_uuid(service_uuid);
  /* Marked active BEFORE starting: starting runs WiFi's start event,
   * whose handler (wifi.c) must already know that setup is running. */
  active = true;
  esp_err_t err = network_prov_mgr_start_provisioning(NETWORK_PROV_SECURITY_2, &sec2_params, name, NULL);
  if (err != ESP_OK) {
    active = false;
    ESP_LOGE(TAG, "could not start Bluetooth setup: %s", esp_err_to_name(err));
    return err;
  }
  *started = true;
  ESP_LOGI(TAG, "no WiFi yet: set up over Bluetooth as \"%s\" with the pairing code", name);
  return ESP_OK;
}

bool provision_active(void)
{
  return active;
}

void provision_stop(void)
{
  if (active) {
    network_prov_mgr_stop_provisioning(); /* NETWORK_PROV_END follows */
  }
}

/* Polls the BOOT button every 50 ms. (It's the chip's boot-mode pin, but
 * after start it's an ordinary input with a pull-up: pressed = low.) */
static void button_task(void *arg)
{
  int held_ms = 0;
  for (;;) {
    vTaskDelay(pdMS_TO_TICKS(50));
    if (gpio_get_level(BOOT_GPIO) == 0) {
      held_ms += 50;
      if (held_ms == 1000) {
        ESP_LOGI(TAG, "BOOT held: keep holding for 4 more seconds to forget the WiFi");
      }
      if (held_ms >= HOLD_MS) {
        ESP_LOGW(TAG, "forgetting the WiFi, restarting into Bluetooth setup");
        network_prov_mgr_reset_wifi_provisioning();
        esp_wifi_restore(); /* the stored network, too, whoever set it */
        vTaskDelay(pdMS_TO_TICKS(200));
        esp_restart();
      }
    } else {
      held_ms = 0;
    }
  }
}

esp_err_t provision_watch_button(void)
{
  gpio_config_t io = {
    .pin_bit_mask = 1ULL << BOOT_GPIO,
    .mode = GPIO_MODE_INPUT,
    .pull_up_en = GPIO_PULLUP_ENABLE,
  };
  ESP_RETURN_ON_ERROR(gpio_config(&io), TAG, "BOOT pin");
  BaseType_t ok = xTaskCreate(button_task, "boot_button", 3072, NULL, 2, NULL);
  return ok == pdPASS ? ESP_OK : ESP_ERR_NO_MEM;
}
