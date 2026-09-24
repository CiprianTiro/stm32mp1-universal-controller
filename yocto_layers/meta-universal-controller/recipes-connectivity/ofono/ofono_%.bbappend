# Installed (Poky's base package groups pull it in for the "3g"/"phone"
# distro features), but not started at boot (issue #59): nothing uses
# telephony yet, and every service started at boot competes with the UI
# for the DK2's two cores. Kept installed rather than removed, so the LTE
# uplink (#55) can simply enable it again.
SYSTEMD_AUTO_ENABLE:${PN} = "disable"
