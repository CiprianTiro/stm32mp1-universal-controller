idf_component_register(SRCS "main.c" "identity.c" "wifi.c" "provision.c" "hub_link.c" "code_json.c"
                            "ir_service.c" "ir_nec.c" "ir_rx.c" "ir_tx.c"
                       INCLUDE_DIRS "."
                       PRIV_REQUIRES esp_driver_rmt esp_driver_gpio esp_timer console
                                     nvs_flash esp_wifi esp_netif esp_event mbedtls lwip protocomm)
