idf_component_register(SRCS "main.c" "sensor.c" "sensor_link.c"
                       INCLUDE_DIRS "."
                       # uc_addon: identity, WiFi, Bluetooth setup, the hub link and
                       # the shared console commands (firmware_common/, #86).
                       PRIV_REQUIRES uc_addon esp_driver_i2c esp_driver_gpio esp_timer console nvs_flash)
