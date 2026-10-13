/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 *
 * uc_addon -- see uc_addon.h.
 */

#include "uc_addon.h"

#include <stddef.h>

static const uc_addon_config_t *current;

void uc_addon_init(const uc_addon_config_t *config)
{
  current = config;
}

const uc_addon_config_t *uc_addon_config(void)
{
  return current;
}
