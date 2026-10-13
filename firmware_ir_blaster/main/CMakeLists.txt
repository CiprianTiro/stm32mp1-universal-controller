idf_component_register(SRCS "main.c" "ir_link.c" "code_json.c"
                            "ir_service.c" "ir_nec.c" "ir_rx.c" "ir_tx.c"
                       INCLUDE_DIRS "."
                       # uc_addon: identity, WiFi, Bluetooth setup and the hub link,
                       # shared with the other add-ons (firmware_common/, #86).
                       PRIV_REQUIRES uc_addon esp_driver_rmt esp_driver_gpio esp_timer console
                                     nvs_flash esp_wifi)
