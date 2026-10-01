/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * Identity -- see identity.h.
 *
 * Mbed TLS 4 (the version in ESP-IDF v6) does its cryptography through the
 * "PSA Crypto" API: keys are generated with psa_generate_key(), random
 * numbers come from psa_generate_random(), and there is no separate random
 * generator to pass around any more. The older Mbed TLS examples found
 * online (mbedtls_ctr_drbg_..., mbedtls_pk_setup + mbedtls_ecp_gen_key)
 * don't apply.
 *
 * What's stored, in the NVS namespace "identity":
 *   "key"   the private key, DER (PKCS#8-like, as mbedtls_pk_write_key_der)
 *   "cert"  our certificate, DER
 *   "code"  the pairing code, 16 characters
 *   "hub"   the paired hub's fingerprint, 64 hex digits (absent = unpaired)
 */

#include "identity.h"

#include <stdio.h>
#include <string.h>
#include "esp_check.h"
#include "esp_log.h"
#include "esp_mac.h"
#include "mbedtls/x509_crt.h"
#include "nvs.h"
#include "psa/crypto.h"

static const char *TAG = "identity";

#define NVS_NAMESPACE "identity"

/* Crockford base32: 32 symbols that can't be mixed up when read from a
 * label (no I/L like 1, no O like 0, no U). 5 bits per character. */
static const char CODE_ALPHABET[] = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/* Big enough for a P-256 key or a small self-signed certificate in DER. */
#define DER_MAX 1024

static char device_id[20];
static char pairing_code[IDENTITY_CODE_LEN];
static char fingerprint[IDENTITY_FINGERPRINT_LEN];
static char hub_fingerprint[IDENTITY_FINGERPRINT_LEN]; /* "" = unpaired */

static mbedtls_pk_context key;
static mbedtls_x509_crt cert;

void identity_sha256_hex(const unsigned char *data, size_t len, char out[IDENTITY_FINGERPRINT_LEN])
{
  unsigned char hash[32];
  size_t hash_len = 0;

  if (psa_hash_compute(PSA_ALG_SHA_256, data, len, hash, sizeof(hash), &hash_len) != PSA_SUCCESS) {
    out[0] = '\0';
    return;
  }
  for (int i = 0; i < 32; i++) {
    sprintf(&out[i * 2], "%02x", hash[i]);
  }
}

/* ---------------------------------------------------------------------------
 * First start: make everything
 * ------------------------------------------------------------------------- */

/* A new ECDSA P-256 key, generated inside PSA, then copied into an Mbed TLS
 * "pk" context (what the certificate writer and the TLS stack take). The
 * key must be EXPORTABLE once, so we can write it to flash. */
static esp_err_t make_key(mbedtls_pk_context *pk)
{
  psa_key_attributes_t attributes = PSA_KEY_ATTRIBUTES_INIT;
  psa_set_key_type(&attributes, PSA_KEY_TYPE_ECC_KEY_PAIR(PSA_ECC_FAMILY_SECP_R1));
  psa_set_key_bits(&attributes, 256);
  psa_set_key_usage_flags(&attributes, PSA_KEY_USAGE_SIGN_HASH | PSA_KEY_USAGE_EXPORT);
  psa_set_key_algorithm(&attributes, PSA_ALG_ECDSA(PSA_ALG_ANY_HASH));

  mbedtls_svc_key_id_t key_id;
  if (psa_generate_key(&attributes, &key_id) != PSA_SUCCESS) {
    return ESP_FAIL;
  }
  int ret = mbedtls_pk_copy_from_psa(key_id, pk);
  psa_destroy_key(key_id); /* the pk context has its own copy now */
  return ret == 0 ? ESP_OK : ESP_FAIL;
}

/* Our self-signed certificate, DER, written at the END of `buf` (that's
 * how Mbed TLS writes DER); returns its length, or <= 0 on error. */
static int make_cert(mbedtls_pk_context *pk, unsigned char *buf, size_t size)
{
  char subject[64];
  snprintf(subject, sizeof(subject), "CN=%s,O=Universal Controller", device_id);

  /* A random, positive serial number (top bit clear). */
  unsigned char serial[8];
  psa_generate_random(serial, sizeof(serial));
  serial[0] &= 0x7F;

  mbedtls_x509write_cert crt;
  mbedtls_x509write_crt_init(&crt);
  mbedtls_x509write_crt_set_version(&crt, MBEDTLS_X509_CRT_VERSION_3);
  mbedtls_x509write_crt_set_serial_raw(&crt, serial, sizeof(serial));
  /* The dates don't matter: the hub checks the fingerprint, not the
   * validity (the blaster has no reliable clock anyway). Wide on purpose. */
  mbedtls_x509write_crt_set_validity(&crt, "20260101000000", "20991231235959");
  mbedtls_x509write_crt_set_subject_name(&crt, subject);
  mbedtls_x509write_crt_set_issuer_name(&crt, subject);
  mbedtls_x509write_crt_set_subject_key(&crt, pk);
  mbedtls_x509write_crt_set_issuer_key(&crt, pk); /* self-signed */
  mbedtls_x509write_crt_set_md_alg(&crt, MBEDTLS_MD_SHA256);

  int len = mbedtls_x509write_crt_der(&crt, buf, size);
  mbedtls_x509write_crt_free(&crt);
  return len;
}

/* 16 random characters from CODE_ALPHABET. 16 x 5 bits = 80 bits: far too
 * many to guess, even offline. */
static void make_code(char out[IDENTITY_CODE_LEN])
{
  unsigned char random[16];
  psa_generate_random(random, sizeof(random));
  for (int i = 0; i < 16; i++) {
    out[i] = CODE_ALPHABET[random[i] & 0x1F]; /* 5 bits each */
  }
  out[16] = '\0';
}

static esp_err_t create(nvs_handle_t nvs)
{
  static unsigned char der[DER_MAX]; /* static: keeps 1 KB off the stack */

  ESP_LOGI(TAG, "first start: creating this blaster's key, certificate and pairing code...");

  mbedtls_pk_context pk;
  mbedtls_pk_init(&pk);
  ESP_RETURN_ON_ERROR(make_key(&pk), TAG, "generate key");

  /* Key -> flash. Like the certificate, DER is written at the end. */
  int len = mbedtls_pk_write_key_der(&pk, der, sizeof(der));
  if (len <= 0) {
    mbedtls_pk_free(&pk);
    return ESP_FAIL;
  }
  ESP_RETURN_ON_ERROR(nvs_set_blob(nvs, "key", der + sizeof(der) - len, len), TAG, "store key");

  /* Certificate -> flash. */
  len = make_cert(&pk, der, sizeof(der));
  mbedtls_pk_free(&pk);
  if (len <= 0) {
    ESP_LOGE(TAG, "certificate: error -0x%04x", -len);
    return ESP_FAIL;
  }
  ESP_RETURN_ON_ERROR(nvs_set_blob(nvs, "cert", der + sizeof(der) - len, len), TAG, "store cert");

  /* Pairing code -> flash. */
  char code[IDENTITY_CODE_LEN];
  make_code(code);
  ESP_RETURN_ON_ERROR(nvs_set_str(nvs, "code", code), TAG, "store code");

  return nvs_commit(nvs);
}

/* ---------------------------------------------------------------------------
 * Every start: load
 * ------------------------------------------------------------------------- */

esp_err_t identity_init(void)
{
  static unsigned char der[DER_MAX];

  /* PSA must be started before any key or random-number function. Calling
   * it again when ESP-IDF already did is harmless. */
  if (psa_crypto_init() != PSA_SUCCESS) {
    return ESP_FAIL;
  }

  uint8_t mac[6];
  ESP_RETURN_ON_ERROR(esp_read_mac(mac, ESP_MAC_WIFI_STA), TAG, "read MAC");
  snprintf(device_id, sizeof(device_id), "irb-%02x%02x%02x%02x%02x%02x",
           mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

  nvs_handle_t nvs;
  ESP_RETURN_ON_ERROR(nvs_open(NVS_NAMESPACE, NVS_READWRITE, &nvs), TAG, "open NVS");

  size_t len = sizeof(der);
  if (nvs_get_blob(nvs, "key", der, &len) == ESP_ERR_NVS_NOT_FOUND) {
    esp_err_t err = create(nvs);
    if (err != ESP_OK) {
      nvs_close(nvs);
      return err;
    }
  }

  /* The key. */
  len = sizeof(der);
  esp_err_t err = nvs_get_blob(nvs, "key", der, &len);
  mbedtls_pk_init(&key);
  if (err != ESP_OK || mbedtls_pk_parse_key(&key, der, len, NULL, 0) != 0) {
    nvs_close(nvs);
    ESP_LOGE(TAG, "stored key unreadable -- factory reset needed");
    return ESP_FAIL;
  }

  /* The certificate, and its fingerprint. */
  len = sizeof(der);
  err = nvs_get_blob(nvs, "cert", der, &len);
  mbedtls_x509_crt_init(&cert);
  if (err != ESP_OK || mbedtls_x509_crt_parse_der(&cert, der, len) != 0) {
    nvs_close(nvs);
    ESP_LOGE(TAG, "stored certificate unreadable -- factory reset needed");
    return ESP_FAIL;
  }
  identity_sha256_hex(der, len, fingerprint);

  /* The pairing code, and the hub (if paired). */
  len = sizeof(pairing_code);
  err = nvs_get_str(nvs, "code", pairing_code, &len);
  if (err != ESP_OK) {
    nvs_close(nvs);
    return err;
  }
  len = sizeof(hub_fingerprint);
  if (nvs_get_str(nvs, "hub", hub_fingerprint, &len) != ESP_OK) {
    hub_fingerprint[0] = '\0';
  }

  nvs_close(nvs);
  ESP_LOGI(TAG, "%s, certificate %.16s..., %s", device_id, fingerprint,
           hub_fingerprint[0] ? "paired" : "not paired");
  return ESP_OK;
}

/* ---------------------------------------------------------------------------
 * Accessors and the hub
 * ------------------------------------------------------------------------- */

const char *identity_device_id(void) { return device_id; }
const char *identity_pairing_code(void) { return pairing_code; }
mbedtls_x509_crt *identity_cert(void) { return &cert; }
mbedtls_pk_context *identity_key(void) { return &key; }
const char *identity_fingerprint(void) { return fingerprint; }

void identity_pairing_code_display(char out[20])
{
  /* "ABCDEFGHJKMNPQRS" -> "ABCD-EFGH-JKMN-PQRS" */
  int o = 0;
  for (int i = 0; i < 16; i++) {
    if (i > 0 && i % 4 == 0) {
      out[o++] = '-';
    }
    out[o++] = pairing_code[i];
  }
  out[o] = '\0';
}

const char *identity_hub_fingerprint(void)
{
  return hub_fingerprint[0] ? hub_fingerprint : NULL;
}

static esp_err_t store_hub(const char *value)
{
  nvs_handle_t nvs;
  ESP_RETURN_ON_ERROR(nvs_open(NVS_NAMESPACE, NVS_READWRITE, &nvs), TAG, "open NVS");
  esp_err_t err = value ? nvs_set_str(nvs, "hub", value) : nvs_erase_key(nvs, "hub");
  if (err == ESP_ERR_NVS_NOT_FOUND) {
    err = ESP_OK; /* erasing what isn't there */
  }
  if (err == ESP_OK) {
    err = nvs_commit(nvs);
  }
  nvs_close(nvs);
  return err;
}

esp_err_t identity_set_hub(const char *hub)
{
  if (strlen(hub) != IDENTITY_FINGERPRINT_LEN - 1) {
    return ESP_ERR_INVALID_ARG;
  }
  ESP_RETURN_ON_ERROR(store_hub(hub), TAG, "store hub");
  strcpy(hub_fingerprint, hub);
  return ESP_OK;
}

esp_err_t identity_clear_hub(void)
{
  ESP_RETURN_ON_ERROR(store_hub(NULL), TAG, "forget hub");
  hub_fingerprint[0] = '\0';
  return ESP_OK;
}
