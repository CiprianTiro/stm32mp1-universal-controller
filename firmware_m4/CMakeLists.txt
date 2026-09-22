cmake_minimum_required(VERSION 3.20.0)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(hello_m4)

# LIBMETAL_INCLUDE_DIR / OPENAMP_INCLUDE_DIR are set by those modules' own
# CMakeLists once CONFIG_OPENAMP=y pulls them in - matches Zephyr's own
# openamp_rsc_table sample, which is the only sample with board files
# proven for stm32mp157c_dk2 (see prj.conf).
target_include_directories(app PRIVATE ${LIBMETAL_INCLUDE_DIR} ${OPENAMP_INCLUDE_DIR})

target_sources(app PRIVATE src/main.c)
