/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * ===========================================================================
 * identity: who this blaster is, and which hub it belongs to (#42)
 * ===========================================================================
 *
 * Made once, on the very first start, and kept in flash (NVS) from then on:
 *
 *   device id       "irb-<mac>", e.g. "irb-7ce8b1b091e4" -- from the chip's
 *                   factory MAC, so it's the same after a reflash
 *   key             an ECDSA P-256 private key, generated ON the chip; it
 *                   never leaves it (only the TLS handshake uses it)
 *   certificate     self-signed, for that key; its SHA-256 is the
 *                   fingerprint the hub pins at pairing
 *   pairing code    16 random characters (80 bits), shown as
 *                   XXXX-XXXX-XXXX-XXXX; proves to the hub that the person
 *                   pairing has this blaster in hand (PROTOCOL.md)
 *   hub fingerprint the one hub allowed to connect, once paired; empty
 *                   while unpaired
 *
 * The key, certificate and code stay the same until a factory reset, so
 * a printed pairing-code label stays valid and a paired hub keeps
 * recognising the blaster after a firmware update.
 */

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include "esp_err.h"
#include "mbedtls/pk.h"
#include "mbedtls/x509_crt.h"

/* A SHA-256 fingerprint as text: 64 hex digits + terminating zero. */
#define IDENTITY_FINGERPRINT_LEN 65

/* Pairing code without dashes: 16 characters + terminating zero. */
#define IDENTITY_CODE_LEN 17

/* Loads the identity from flash, creating it first on the very first
 * start (takes a second or two: key generation). Needs NVS initialised. */
esp_err_t identity_init(void);

/* "irb-7ce8b1b091e4" */
const char *identity_device_id(void);

/* The pairing code without dashes, upper case ("7K3M...", 16 chars). */
const char *identity_pairing_code(void);

/* The same code as people read it: "7K3M-Q9XW-..." (20 chars + zero). */
void identity_pairing_code_display(char out[20]);

/* Our certificate, parsed, and its private key -- for the TLS server. */
mbedtls_x509_crt *identity_cert(void);
mbedtls_pk_context *identity_key(void);

/* Our certificate's fingerprint (FB in PROTOCOL.md). */
const char *identity_fingerprint(void);

/* The paired hub's fingerprint (FH), or NULL while unpaired. */
const char *identity_hub_fingerprint(void);

/* Pairing succeeded: remember this hub. */
esp_err_t identity_set_hub(const char *fingerprint);

/* Forget the hub (unpair). Key, certificate and code stay. */
esp_err_t identity_clear_hub(void);

/* SHA-256 of `data` as 64 lowercase hex digits. */
void identity_sha256_hex(const unsigned char *data, size_t len, char out[IDENTITY_FINGERPRINT_LEN]);
