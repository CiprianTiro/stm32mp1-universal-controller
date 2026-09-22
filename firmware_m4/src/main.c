/*
 * Copyright (c) 2026 Ciprian Tironeac
 * SPDX-License-Identifier: Apache-2.0
 */

#include <zephyr/kernel.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/sys/printk.h>

#define HEARTBEAT_PERIOD_MS 1000

static const struct gpio_dt_spec led = GPIO_DT_SPEC_GET(DT_ALIAS(led0), gpios);

int main(void)
{
  if (!gpio_is_ready_dt(&led))
  {
    printk("Heartbeat LED device not ready\n");
    return 0;
  }

  gpio_pin_configure_dt(&led, GPIO_OUTPUT_INACTIVE);

  uint32_t beat = 0;

  while (1)
  {
    gpio_pin_toggle_dt(&led);
    printk("M4 heartbeat: %u\n", beat++);
    k_msleep(HEARTBEAT_PERIOD_MS);
  }

  return 0;
}
