# Issue #72: the hub's LOCAL MQTT broker, for devices that connect to the
# hub (WLED, Tasmota, ESPHome...) -- not the cloud link (#26, mqtt.rs).
#
# Mosquitto from meta-networking, with the hub's own config and unit:
#   - no WebSockets (no libwebsockets in the image), TLS and systemd on;
#   - every device logs in with its OWN user, and may only use its own
#     topics: backend-daemon (broker.rs) writes the password and access
#     files and asks hub-helper to reload;
#   - runs as user mosquitto from the start (upstream's unit starts as
#     root and drops privileges), sandboxed like the hub's other services.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
SRC_URI += "file://hub-mosquitto.conf file://hub-mosquitto.service"

PACKAGECONFIG = "ssl systemd"

do_install:append() {
    install -m 0644 ${WORKDIR}/hub-mosquitto.conf ${D}${sysconfdir}/mosquitto/mosquitto.conf
    install -m 0644 ${WORKDIR}/hub-mosquitto.service ${D}${systemd_unitdir}/system/mosquitto.service
}
