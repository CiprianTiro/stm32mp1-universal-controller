# Point /etc/resolv.conf at systemd-resolved's local stub (127.0.0.53)
# instead of the raw list of upstream servers ("uplink" mode, poky's default).
#
# Why: with the public DNS drop-in (resolved-public-dns recipe), the raw list
# starts with 1.1.1.1/8.8.8.8, and programs only ever try the first three
# entries -- so the router's DNS would never be used, and on a network that
# blocks public DNS every lookup would time out. Through the stub, resolved
# itself asks ALL servers (router's + public) and returns the first
# successful answer, which is the behaviour the drop-in is meant to give.
#
# RESOLV_CONF is poky's own switch for this (see systemd_*.bb do_install);
# it only changes a symlink, so systemd doesn't need to be recompiled.
RESOLV_CONF:stm32mp1common = "stub-resolv"
