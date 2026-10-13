# uc_addon: what the hub's ESP32 add-ons share (#42, #86) -- identity,
# WiFi, Bluetooth setup and the paired hub link. uc_addon.h says more.
# An add-on's project pulls it in with
#   set(EXTRA_COMPONENT_DIRS ../firmware_common)
# in its top-level CMakeLists.txt.
idf_component_register(SRCS "uc_addon.c" "identity.c" "wifi.c" "provision.c" "hub_link.c" "addon_console.c"
                       INCLUDE_DIRS "."
                       # Public: hub_link.h uses cJSON, identity.h Mbed TLS types.
                       REQUIRES mbedtls esp_event
                       PRIV_REQUIRES esp_driver_gpio esp_timer nvs_flash esp_wifi esp_netif lwip protocomm console)
