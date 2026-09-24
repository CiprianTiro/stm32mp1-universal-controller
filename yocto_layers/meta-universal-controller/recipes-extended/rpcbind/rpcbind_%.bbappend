# Installed (Poky's base package groups pull it in for the "nfs" distro
# feature), but not started at boot (issue #59): rpcbind is only needed by
# userspace NFS mounts, which nothing on the hub does. (Netboot's NFS root
# is mounted by the kernel itself, without rpcbind.) Disables both its
# service and its socket.
SYSTEMD_AUTO_ENABLE:${PN} = "disable"
