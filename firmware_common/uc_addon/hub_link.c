/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * Hub link -- see hub_link.h and firmware_ir_blaster/PROTOCOL.md
 * ===========================================================================
 *
 * (Moved here from firmware_ir_blaster/ for #86: the blaster and the
 * sensor share it. What only the blaster does -- send, learn, "heard" --
 * moved to firmware_ir_blaster/main/ir_link.c.)
 *
 * One task does everything here, for one hub connection at a time:
 *
 *   wait for a TCP connection on port 7443
 *     -> TLS handshake (Mbed TLS), the hub must show a certificate
 *     -> check it: the pinned hub? an unpaired add-on? a stranger?
 *     -> loop, every 100 ms:
 *          read what the hub sent, answer each complete line
 *          the add-on's poll() hook (its events, late replies)
 *          idle timeout, a newer connection waiting?
 *
 * Why ONE task that polls, instead of a reader task plus writers: an Mbed
 * TLS connection must not be read and written by two tasks at the same
 * time. With a single owner there's nothing to lock. Reads therefore wait
 * at most 100 ms (a "read timeout"), so the loop keeps turning and can
 * also write events.
 */

#include "hub_link.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include "cJSON.h"
#include "esp_check.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "identity.h"
#include "lwip/sockets.h"
#include "mbedtls/net_sockets.h" /* only for its error codes */
#include "mbedtls/ssl.h"
#include "mdns.h"
#include "psa/crypto.h"
#include "uc_addon.h"

static const char *TAG = "hub_link";

#define MDNS_PROTO "_tcp"

/* Longest request line accepted (PROTOCOL.md). A raw code of 1024 timings
 * is ~7 KB of JSON. */
#define REQUEST_LINE_MAX (16 * 1024)

/* How long one read waits before the loop turns (see the header). */
#define POLL_MS 100

/* A handshake that takes longer than this is abandoned. */
#define HANDSHAKE_TIMEOUT_MS 10000

/* The hub pings every 20 s; nothing for this long = the hub is gone
 * (e.g. its network cable was pulled, so the TCP connection never closed). */
#define IDLE_LIMIT_US (60LL * 1000 * 1000)

/* Wrong pairing proofs allowed before pairing is locked until restart:
 * enough for typos, far too few to guess an 80-bit code. */
#define PAIR_MAX_FAILS 5

struct hub_session {
  int fd;
  mbedtls_ssl_context ssl;
  /* The connected hub may do everything (it's the pinned hub, or pairing
   * just succeeded). Before that only hello and pair are allowed. */
  bool authorised;
  char peer_fingerprint[IDENTITY_FINGERPRINT_LEN];
  int64_t last_rx_us;
};
typedef struct hub_session session_t;

static mbedtls_ssl_config tls_config;
static int listen_fd = -1;
static const uc_addon_app_t *app;
static volatile bool session_active; /* an authorised hub is connected */
static volatile bool drop_session;   /* hub_link_pairing_changed() */
static int pair_fails;

/* Big buffers, static so they're not on the task's stack. Only this task
 * uses them. */
static char line[REQUEST_LINE_MAX + 1];

/* ---------------------------------------------------------------------------
 * Sockets under TLS
 *
 * Mbed TLS doesn't do any networking itself: it calls these two functions
 * to move its encrypted bytes. `ctx` points to the socket number.
 * ------------------------------------------------------------------------- */

static int bio_send(void *ctx, const unsigned char *buf, size_t len)
{
  int n = send(*(int *)ctx, buf, len, 0);
  if (n >= 0) {
    return n;
  }
  return errno == EAGAIN || errno == EWOULDBLOCK ? MBEDTLS_ERR_SSL_WANT_WRITE : MBEDTLS_ERR_NET_SEND_FAILED;
}

/* Waits up to `timeout_ms` for bytes (select), then reads what's there.
 * MBEDTLS_ERR_SSL_TIMEOUT tells Mbed TLS "nothing yet, ask again later". */
static int bio_recv_timeout(void *ctx, unsigned char *buf, size_t len, uint32_t timeout_ms)
{
  int fd = *(int *)ctx;
  fd_set readable;
  FD_ZERO(&readable);
  FD_SET(fd, &readable);
  struct timeval tv = { .tv_sec = timeout_ms / 1000, .tv_usec = (timeout_ms % 1000) * 1000 };

  int ready = select(fd + 1, &readable, NULL, NULL, timeout_ms ? &tv : NULL);
  if (ready == 0) {
    return MBEDTLS_ERR_SSL_TIMEOUT;
  }
  if (ready < 0) {
    return MBEDTLS_ERR_NET_RECV_FAILED;
  }
  int n = recv(fd, buf, len, 0);
  if (n >= 0) {
    return n; /* 0 = the hub closed the connection */
  }
  return errno == EAGAIN || errno == EWOULDBLOCK ? MBEDTLS_ERR_SSL_WANT_READ : MBEDTLS_ERR_NET_RECV_FAILED;
}

/* Called by Mbed TLS for the hub's certificate. We don't trust
 * certificates through an authority: after the handshake we compare the
 * certificate's fingerprint ourselves (check_peer). So: accept here. */
static int accept_any_certificate(void *ctx, mbedtls_x509_crt *crt, int depth, uint32_t *flags)
{
  *flags = 0;
  return 0;
}

/* ---------------------------------------------------------------------------
 * Writing messages
 * ------------------------------------------------------------------------- */

static bool write_all(session_t *s, const char *data, size_t len)
{
  while (len > 0) {
    int n = mbedtls_ssl_write(&s->ssl, (const unsigned char *)data, len);
    if (n == MBEDTLS_ERR_SSL_WANT_WRITE || n == MBEDTLS_ERR_SSL_WANT_READ) {
      vTaskDelay(1);
      continue;
    }
    if (n < 0) {
      return false;
    }
    data += n;
    len -= n;
  }
  return true;
}

/* Sends `json` as one line and frees it. */
static bool write_json(session_t *s, cJSON *json)
{
  char *text = json ? cJSON_PrintUnformatted(json) : NULL;
  cJSON_Delete(json);
  if (!text) {
    return false;
  }
  size_t len = strlen(text);
  char *with_newline = realloc(text, len + 2);
  if (!with_newline) {
    free(text);
    return false;
  }
  with_newline[len] = '\n';
  with_newline[len + 1] = '\0';
  bool ok = write_all(s, with_newline, len + 1);
  free(with_newline);
  return ok;
}

cJSON *hub_link_reply_ok(double id)
{
  cJSON *reply = cJSON_CreateObject();
  cJSON_AddNumberToObject(reply, "id", id);
  cJSON_AddBoolToObject(reply, "ok", true);
  return reply;
}

static bool reply_error(session_t *s, const cJSON *id, const char *kind, const char *detail)
{
  cJSON *reply = cJSON_CreateObject();
  if (cJSON_IsNumber(id)) {
    cJSON_AddNumberToObject(reply, "id", id->valuedouble);
  }
  cJSON_AddBoolToObject(reply, "ok", false);
  cJSON_AddStringToObject(reply, "error", kind);
  if (detail) {
    cJSON_AddStringToObject(reply, "detail", detail);
  }
  return write_json(s, reply);
}

bool hub_link_reply_error(hub_session_t *s, double id, const char *kind, const char *detail)
{
  cJSON *number = cJSON_CreateNumber(id);
  bool ok = reply_error(s, number, kind, detail);
  cJSON_Delete(number);
  return ok;
}

bool hub_link_send(hub_session_t *s, cJSON *msg)
{
  return write_json(s, msg);
}

/* Tells the add-on's code that the paired hub came or went. */
static void set_session_active(bool active)
{
  session_active = active;
  if (app && app->session_changed) {
    app->session_changed(active);
  }
}

/* ---------------------------------------------------------------------------
 * Pairing (PROTOCOL.md, "Pairing")
 * ------------------------------------------------------------------------- */

static void mdns_update_paired(void)
{
  mdns_service_txt_item_set(uc_addon_config()->mdns_service, MDNS_PROTO, "paired", identity_hub_fingerprint() ? "1" : "0");
}

/* HMAC-SHA256(code, "<label> <role> <FB> <FH>") as hex, the label being
 * uc_addon.h's pair_label ("uc-irb-pair-v1" for the blaster); "" on
 * error. Mbed TLS 4 computes HMACs through PSA: the code becomes a
 * short-lived HMAC key, used once, then destroyed. */
static void pair_proof(const char *role, const char *hub_fp, char out[IDENTITY_FINGERPRINT_LEN])
{
  char message[192];
  unsigned char mac[32];
  size_t mac_len = 0;

  out[0] = '\0';
  snprintf(message, sizeof(message), "%s %s %s %s", uc_addon_config()->pair_label, role,
           identity_fingerprint(), hub_fp);
  const char *code = identity_pairing_code();

  psa_key_attributes_t attributes = PSA_KEY_ATTRIBUTES_INIT;
  psa_set_key_type(&attributes, PSA_KEY_TYPE_HMAC);
  psa_set_key_usage_flags(&attributes, PSA_KEY_USAGE_SIGN_MESSAGE);
  psa_set_key_algorithm(&attributes, PSA_ALG_HMAC(PSA_ALG_SHA_256));
  mbedtls_svc_key_id_t key;
  if (psa_import_key(&attributes, (const uint8_t *)code, strlen(code), &key) != PSA_SUCCESS) {
    return;
  }
  psa_status_t status = psa_mac_compute(key, PSA_ALG_HMAC(PSA_ALG_SHA_256), (const uint8_t *)message,
                                        strlen(message), mac, sizeof(mac), &mac_len);
  psa_destroy_key(key);
  if (status != PSA_SUCCESS || mac_len != sizeof(mac)) {
    return;
  }
  for (int i = 0; i < 32; i++) {
    sprintf(&out[i * 2], "%02x", mac[i]);
  }
}

/* Compares in constant time: how long it takes doesn't reveal how many
 * leading characters matched (which would help guess a proof). */
static bool same_secret(const char *a, const char *b, size_t len)
{
  unsigned char diff = 0;
  for (size_t i = 0; i < len; i++) {
    diff |= (unsigned char)a[i] ^ (unsigned char)b[i];
  }
  return diff == 0;
}

static bool op_pair(session_t *s, const cJSON *id, const cJSON *req)
{
  if (identity_hub_fingerprint()) {
    return reply_error(s, id, "already_paired", "unpair first (from the hub, or `unpair` on the console)");
  }
  if (pair_fails >= PAIR_MAX_FAILS) {
    return reply_error(s, id, "locked", "too many wrong pairing codes; restart the add-on");
  }

  const cJSON *proof = cJSON_GetObjectItemCaseSensitive(req, "proof");
  char expected[IDENTITY_FINGERPRINT_LEN];
  pair_proof("hub", s->peer_fingerprint, expected);
  if (!cJSON_IsString(proof) || strlen(proof->valuestring) != 64 ||
      !same_secret(proof->valuestring, expected, 64)) {
    pair_fails++;
    ESP_LOGW(TAG, "pairing refused: wrong code (%d of %d tries)", pair_fails, PAIR_MAX_FAILS);
    return reply_error(s, id, "bad_proof", "wrong pairing code");
  }

  if (identity_set_hub(s->peer_fingerprint) != ESP_OK) {
    return reply_error(s, id, "failed", "could not store the hub");
  }
  s->authorised = true;
  set_session_active(true);
  pair_fails = 0;
  mdns_update_paired();
  ESP_LOGI(TAG, "paired with hub %.16s...", s->peer_fingerprint);

  char ours[IDENTITY_FINGERPRINT_LEN];
  pair_proof(uc_addon_config()->pair_role, s->peer_fingerprint, ours);
  cJSON *reply = hub_link_reply_ok(id->valuedouble);
  cJSON_AddStringToObject(reply, "proof", ours);
  return write_json(s, reply);
}

/* ---------------------------------------------------------------------------
 * The other requests
 * ------------------------------------------------------------------------- */

static bool op_hello(session_t *s, const cJSON *id)
{
  cJSON *reply = hub_link_reply_ok(id->valuedouble);
  cJSON_AddStringToObject(reply, "device", identity_device_id());
  cJSON_AddStringToObject(reply, "fw", uc_addon_config()->fw_version);
  cJSON_AddNumberToObject(reply, "proto", uc_addon_config()->protocol_version);
  cJSON_AddBoolToObject(reply, "paired", identity_hub_fingerprint() != NULL);
  cJSON_AddStringToObject(reply, "fingerprint", identity_fingerprint());
  return write_json(s, reply);
}

static bool op_unpair(session_t *s, const cJSON *id)
{
  identity_clear_hub();
  mdns_update_paired();
  ESP_LOGI(TAG, "unpaired by the hub");
  write_json(s, hub_link_reply_ok(id->valuedouble));
  return false; /* close the connection: this hub isn't ours any more */
}

/* Handles one request line. Returns false to close the connection. */
static bool handle_line(session_t *s, char *text)
{
  cJSON *req = cJSON_Parse(text);
  if (!req) {
    return reply_error(s, NULL, "bad_request", "not JSON");
  }

  const cJSON *id = cJSON_GetObjectItemCaseSensitive(req, "id");
  const cJSON *op = cJSON_GetObjectItemCaseSensitive(req, "op");
  bool keep = true;

  if (!cJSON_IsNumber(id) || !cJSON_IsString(op)) {
    keep = reply_error(s, id, "bad_request", "needs \"id\" (number) and \"op\" (text)");
  } else if (strcmp(op->valuestring, "hello") == 0) {
    keep = op_hello(s, id);
  } else if (strcmp(op->valuestring, "pair") == 0) {
    keep = op_pair(s, id, req);
  } else if (!s->authorised) {
    keep = reply_error(s, id, "not_paired", "pair first");
  } else if (strcmp(op->valuestring, "ping") == 0) {
    keep = write_json(s, hub_link_reply_ok(id->valuedouble));
  } else if (strcmp(op->valuestring, "unpair") == 0) {
    keep = op_unpair(s, id);
  } else {
    /* The add-on's own requests (the blaster's send / learn, ...). */
    bool handled = false;
    if (app && app->handle) {
      keep = app->handle(s, op->valuestring, id->valuedouble, req, &handled);
    }
    if (!handled) {
      keep = reply_error(s, id, "bad_request", "unknown op");
    }
  }

  cJSON_Delete(req);
  return keep;
}

/* ---------------------------------------------------------------------------
 * One connection
 * ------------------------------------------------------------------------- */

/* A newer connection waiting on the listening socket, or -1. */
static int accept_pending(void)
{
  int fd = accept(listen_fd, NULL, NULL);
  if (fd < 0) {
    return -1; /* EAGAIN: nobody waiting (the listening socket is non-blocking) */
  }
  /* The connection itself: blocking (bio_recv_timeout waits with select),
   * but a send never hangs forever. */
  int flags = fcntl(fd, F_GETFL, 0);
  fcntl(fd, F_SETFL, flags & ~O_NONBLOCK);
  struct timeval send_timeout = { .tv_sec = 5 };
  setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &send_timeout, sizeof(send_timeout));
  return fd;
}

/* After the handshake: who is this? Returns false to refuse. */
static bool check_peer(session_t *s)
{
  const mbedtls_x509_crt *peer = mbedtls_ssl_get_peer_cert(&s->ssl);
  if (!peer) {
    ESP_LOGW(TAG, "refused: the client showed no certificate");
    return false;
  }
  identity_sha256_hex(peer->raw.p, peer->raw.len, s->peer_fingerprint);

  const char *hub = identity_hub_fingerprint();
  if (!hub) {
    ESP_LOGI(TAG, "a hub connected (not paired yet)");
    s->authorised = false;
    return true;
  }
  if (strcmp(hub, s->peer_fingerprint) != 0) {
    ESP_LOGW(TAG, "refused: a hub that isn't ours (%.16s...)", s->peer_fingerprint);
    return false;
  }
  ESP_LOGI(TAG, "hub connected");
  s->authorised = true;
  return true;
}

/* Runs one connection until it ends. Returns the socket of a newer
 * connection that replaced it, or -1. */
static int run_session(int fd)
{
  static session_t s; /* static: the Mbed TLS context is large */
  int next_fd = -1;

  memset(&s, 0, sizeof(s));
  s.fd = fd;
  s.last_rx_us = esp_timer_get_time();
  mbedtls_ssl_init(&s.ssl);
  drop_session = false;

  mbedtls_ssl_conf_read_timeout(&tls_config, HANDSHAKE_TIMEOUT_MS);
  if (mbedtls_ssl_setup(&s.ssl, &tls_config) != 0) {
    goto done;
  }
  mbedtls_ssl_set_bio(&s.ssl, &s.fd, bio_send, NULL, bio_recv_timeout);

  int ret;
  while ((ret = mbedtls_ssl_handshake(&s.ssl)) != 0) {
    if (ret != MBEDTLS_ERR_SSL_WANT_READ && ret != MBEDTLS_ERR_SSL_WANT_WRITE) {
      ESP_LOGW(TAG, "TLS handshake failed: -0x%04x", -ret);
      goto done;
    }
  }
  if (!check_peer(&s)) {
    goto done;
  }
  if (s.authorised) {
    set_session_active(true);
  }
  mbedtls_ssl_conf_read_timeout(&tls_config, POLL_MS);

  size_t line_len = 0;
  for (;;) {
    ret = mbedtls_ssl_read(&s.ssl, (unsigned char *)line + line_len, REQUEST_LINE_MAX - line_len);
    if (ret > 0) {
      s.last_rx_us = esp_timer_get_time();
      line_len += ret;
      /* Answer every complete line; keep a partial one for later. */
      char *start = line;
      char *newline;
      while ((newline = memchr(start, '\n', line_len - (start - line))) != NULL) {
        *newline = '\0';
        if (newline > start && !handle_line(&s, start)) {
          goto done;
        }
        start = newline + 1;
      }
      line_len -= start - line;
      memmove(line, start, line_len);
      if (line_len == REQUEST_LINE_MAX) {
        ESP_LOGW(TAG, "closing: request line longer than %d bytes", REQUEST_LINE_MAX);
        goto done;
      }
    } else if (ret != MBEDTLS_ERR_SSL_TIMEOUT && ret != MBEDTLS_ERR_SSL_WANT_READ &&
               ret != MBEDTLS_ERR_SSL_WANT_WRITE) {
      /* 0 or MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY: the hub closed it. */
      ESP_LOGI(TAG, "hub disconnected");
      goto done;
    }

    if (s.authorised && app && app->poll && !app->poll(&s)) {
      goto done;
    }
    if (esp_timer_get_time() - s.last_rx_us > IDLE_LIMIT_US) {
      ESP_LOGW(TAG, "hub silent for 60 s, closing");
      goto done;
    }
    if (drop_session) {
      goto done;
    }
    if ((next_fd = accept_pending()) >= 0) {
      ESP_LOGI(TAG, "a new connection replaces the current one");
      goto done;
    }
  }

done:
  if (session_active) {
    set_session_active(false);
  }
  mbedtls_ssl_close_notify(&s.ssl);
  mbedtls_ssl_free(&s.ssl);
  close(fd);
  return next_fd;
}

static void server_task(void *arg)
{
  for (;;) {
    fd_set readable;
    FD_ZERO(&readable);
    FD_SET(listen_fd, &readable);
    if (select(listen_fd + 1, &readable, NULL, NULL, NULL) <= 0) {
      continue;
    }
    int fd = accept_pending();
    while (fd >= 0) {
      fd = run_session(fd);
    }
  }
}

/* ---------------------------------------------------------------------------
 * Start
 * ------------------------------------------------------------------------- */

static esp_err_t start_mdns(void)
{
  ESP_RETURN_ON_ERROR(mdns_init(), TAG, "mdns init");
  ESP_RETURN_ON_ERROR(mdns_hostname_set(identity_device_id()), TAG, "mdns hostname");

  /* "IR blaster 91e4": the device id's last 4 digits tell two apart. */
  char instance[48];
  const char *id = identity_device_id();
  snprintf(instance, sizeof(instance), "%s %s", uc_addon_config()->product, id + strlen(id) - 4);
  ESP_RETURN_ON_ERROR(mdns_instance_name_set(instance), TAG, "mdns instance");

  char proto[4];
  snprintf(proto, sizeof(proto), "%d", uc_addon_config()->protocol_version);
  mdns_txt_item_t txt[] = {
    { "id", id },
    { "fw", uc_addon_config()->fw_version },
    { "proto", proto },
    { "paired", identity_hub_fingerprint() ? "1" : "0" },
  };
  return mdns_service_add(NULL, uc_addon_config()->mdns_service, MDNS_PROTO, HUB_LINK_PORT, txt, 4);
}

esp_err_t hub_link_start(const uc_addon_app_t *add_on)
{
  app = add_on;

  /* TLS settings, shared by every connection. */
  mbedtls_ssl_config_init(&tls_config);
  ESP_RETURN_ON_FALSE(mbedtls_ssl_config_defaults(&tls_config, MBEDTLS_SSL_IS_SERVER,
                                                  MBEDTLS_SSL_TRANSPORT_STREAM,
                                                  MBEDTLS_SSL_PRESET_DEFAULT) == 0,
                      ESP_FAIL, TAG, "TLS defaults");
  ESP_RETURN_ON_FALSE(mbedtls_ssl_conf_own_cert(&tls_config, identity_cert(), identity_key()) == 0,
                      ESP_FAIL, TAG, "TLS own certificate");
  /* Ask the hub for its certificate. OPTIONAL + accept_any_certificate:
   * Mbed TLS takes any certificate, and check_peer() decides by
   * fingerprint. (The "CA chain" is only there because Mbed TLS wants
   * one; no certificate is trusted through it.) */
  mbedtls_ssl_conf_authmode(&tls_config, MBEDTLS_SSL_VERIFY_OPTIONAL);
  mbedtls_ssl_conf_ca_chain(&tls_config, identity_cert(), NULL);
  mbedtls_ssl_conf_verify(&tls_config, accept_any_certificate, NULL);

  /* The listening socket. Non-blocking, so accept_pending() can check
   * for a newer connection without waiting. */
  listen_fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  ESP_RETURN_ON_FALSE(listen_fd >= 0, ESP_FAIL, TAG, "socket");
  int yes = 1;
  setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes));
  struct sockaddr_in addr = {
    .sin_family = AF_INET,
    .sin_port = htons(HUB_LINK_PORT),
    .sin_addr.s_addr = htonl(INADDR_ANY),
  };
  ESP_RETURN_ON_FALSE(bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) == 0, ESP_FAIL, TAG, "bind");
  ESP_RETURN_ON_FALSE(listen(listen_fd, 2) == 0, ESP_FAIL, TAG, "listen");
  fcntl(listen_fd, F_SETFL, fcntl(listen_fd, F_GETFL, 0) | O_NONBLOCK);

  ESP_RETURN_ON_ERROR(start_mdns(), TAG, "mdns");

  /* 10 KB: the TLS handshake (ECDSA) needs a good few KB of stack. */
  BaseType_t ok = xTaskCreate(server_task, "hub_link", 10240, NULL, 4, NULL);
  ESP_RETURN_ON_FALSE(ok == pdPASS, ESP_ERR_NO_MEM, TAG, "task");

  ESP_LOGI(TAG, "waiting for the hub on port %d", HUB_LINK_PORT);
  return ESP_OK;
}

bool hub_link_connected(void)
{
  return session_active;
}

void hub_link_pairing_changed(void)
{
  mdns_update_paired();
  drop_session = true;
}
