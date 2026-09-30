/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * WiFi -- see wifi.h.
 *
 * ESP-IDF's WiFi works through EVENTS: we ask it to start or connect, and
 * it tells us later what happened (started, connected, got an IP address,
 * disconnected) by calling on_event() from its event task. So this file
 * is mostly one event handler that reacts to each of those.
 */

#include "wifi.h"

#include <stdio.h>
#include <string.h>
#include "esp_check.h"
#include "esp_event.h"
#include "esp_log.h"
#include "esp_netif.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "identity.h"

static const char *TAG = "wifi";

/* After a lost connection, wait before trying again: 1 s, then doubling up
 * to 30 s, so a router that's off for an hour isn't hammered. */
#define RETRY_FIRST_MS 1000
#define RETRY_MAX_MS 30000

static esp_netif_t *netif;
static volatile bool connected;
static uint32_t retry_ms = RETRY_FIRST_MS;
static esp_timer_handle_t retry_timer;

static bool has_network(void)
{
  wifi_config_t config;
  return esp_wifi_get_config(WIFI_IF_STA, &config) == ESP_OK && config.sta.ssid[0] != '\0';
}

static void retry(void *arg)
{
  if (has_network()) {
    esp_wifi_connect();
  }
}

static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
  if (base == WIFI_EVENT && id == WIFI_EVENT_STA_START) {
    if (has_network()) {
      esp_wifi_connect();
    } else {
      ESP_LOGW(TAG, "no network configured -- type: wifi <name> <password>");
    }
  } else if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
    wifi_event_sta_disconnected_t *event = data;
    if (connected) {
      ESP_LOGW(TAG, "connection lost (reason %d)", event->reason);
    }
    connected = false;
    /* Try again later (a timer, so this event task isn't blocked). */
    esp_timer_stop(retry_timer);
    esp_timer_start_once(retry_timer, (uint64_t)retry_ms * 1000);
    retry_ms = retry_ms * 2 > RETRY_MAX_MS ? RETRY_MAX_MS : retry_ms * 2;
  } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
    ip_event_got_ip_t *event = data;
    ESP_LOGI(TAG, "connected, address " IPSTR, IP2STR(&event->ip_info.ip));
    connected = true;
    retry_ms = RETRY_FIRST_MS;
  }
}

esp_err_t wifi_start(void)
{
  ESP_RETURN_ON_ERROR(esp_netif_init(), TAG, "netif init");
  ESP_RETURN_ON_ERROR(esp_event_loop_create_default(), TAG, "event loop");
  netif = esp_netif_create_default_wifi_sta();

  /* The name the router shows in its device list. */
  esp_netif_set_hostname(netif, identity_device_id());

  wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
  ESP_RETURN_ON_ERROR(esp_wifi_init(&init), TAG, "wifi init");
  /* Keep the network name and password in flash (the default, said
   * explicitly because the whole setup depends on it). */
  ESP_RETURN_ON_ERROR(esp_wifi_set_storage(WIFI_STORAGE_FLASH), TAG, "storage");

  esp_timer_create_args_t timer_args = { .callback = retry, .name = "wifi_retry" };
  ESP_RETURN_ON_ERROR(esp_timer_create(&timer_args, &retry_timer), TAG, "timer");

  ESP_RETURN_ON_ERROR(esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_event, NULL), TAG, "events");
  ESP_RETURN_ON_ERROR(esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_event, NULL), TAG, "events");

  ESP_RETURN_ON_ERROR(esp_wifi_set_mode(WIFI_MODE_STA), TAG, "mode");
  ESP_RETURN_ON_ERROR(esp_wifi_start(), TAG, "start");

  /* The blaster is mains-powered and must answer the hub quickly: no WiFi
   * power saving (which can add 100+ ms of delay to every message). */
  esp_wifi_set_ps(WIFI_PS_NONE);
  return ESP_OK;
}

esp_err_t wifi_set_network(const char *ssid, const char *password)
{
  wifi_config_t config = { 0 };

  if (strlen(ssid) == 0 || strlen(ssid) >= sizeof(config.sta.ssid) ||
      strlen(password) >= sizeof(config.sta.password)) {
    return ESP_ERR_INVALID_ARG;
  }
  strcpy((char *)config.sta.ssid, ssid);
  strcpy((char *)config.sta.password, password);
  /* With a password, refuse networks weaker than WPA2 -- so a fake open
   * network with the same name can't lure the blaster in. */
  config.sta.threshold.authmode = password[0] ? WIFI_AUTH_WPA2_PSK : WIFI_AUTH_OPEN;

  esp_wifi_disconnect();
  connected = false;
  retry_ms = RETRY_FIRST_MS;
  ESP_RETURN_ON_ERROR(esp_wifi_set_config(WIFI_IF_STA, &config), TAG, "set config");
  return esp_wifi_connect();
}

esp_err_t wifi_forget(void)
{
  wifi_config_t config = { 0 };
  esp_timer_stop(retry_timer);
  esp_wifi_disconnect();
  connected = false;
  return esp_wifi_set_config(WIFI_IF_STA, &config);
}

bool wifi_is_connected(void)
{
  return connected;
}

void wifi_print_status(void)
{
  wifi_config_t config;
  if (esp_wifi_get_config(WIFI_IF_STA, &config) != ESP_OK || config.sta.ssid[0] == '\0') {
    printf("WiFi: no network configured. Type: wifi <name> <password>\n");
    return;
  }
  if (!connected) {
    printf("WiFi: \"%s\", not connected (retrying)\n", (char *)config.sta.ssid);
    return;
  }

  esp_netif_ip_info_t ip;
  wifi_ap_record_t ap;
  esp_netif_get_ip_info(netif, &ip);
  int rssi = esp_wifi_sta_get_ap_info(&ap) == ESP_OK ? ap.rssi : 0;
  printf("WiFi: \"%s\", connected, address " IPSTR ", signal %d dBm\n",
         (char *)config.sta.ssid, IP2STR(&ip.ip), rssi);
}
