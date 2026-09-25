# nftables for the hub's firewall (issue #37, recipes-core/hub-hardening).
# The hub only ever runs `nft -f <rules>` and `nft list ...`: no interactive
# nft shell (readline), no Python bindings, no JSON input/output. Leaving
# them out keeps readline, python3 and jansson off the board -- less code
# that could have a flaw, and less to keep updated.
PACKAGECONFIG = ""
