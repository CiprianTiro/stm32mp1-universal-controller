# ESP32-S3 room sensor firmware (#86). Built with ESP-IDF, see README.md.
cmake_minimum_required(VERSION 3.22)

# uc_addon (identity, WiFi, Bluetooth setup, hub link, console), shared
# with the IR blaster.
set(EXTRA_COMPONENT_DIRS ../firmware_common)

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(room_sensor)
