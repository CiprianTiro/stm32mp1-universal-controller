# The PRODUCTION variant of the hub image (issue #37): exactly the
# development image (universal-controller-image.bb), minus every way in
# that a finished product shouldn't have:
#   - no SSH server at all (and no maintainer key, no SSH port in the
#     firewall): the hub is managed only through its own screen and its
#     authenticated API (wss://:8443, paired clients);
#   - no systemd-analyze (a development/audit tool).
# Build and flash it with `make build-hw-prod` / `make flash-hw-prod`.
# Everything else -- the services, their sandboxing, the firewall -- is the
# same, so what is tested on the development image is what ships.
#
# Consequence to keep in mind: `make flash-m4` and the automatic /usr/local
# backup in `make flash-hw` use SSH, so they don't work on a board running
# this image (flash-hw still keeps /usr/local; it just can't take the extra
# backup copy first).
require universal-controller-image.bb

SUMMARY = "Production image for the STM32MP1 Universal Controller hub (no SSH)"

IMAGE_INSTALL:remove = " \
    openssh \
    openssh-sshd \
    openssh-sftp-server \
    ssh-root-key \
    hub-hardening-ssh \
    systemd-analyze \
"
