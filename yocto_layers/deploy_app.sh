#!/usr/bin/env bash
# =============================================================================
# Fast dev loop (issue #38): rebuild ONE app recipe and put it on a running
# DK2 over SSH -- minutes, instead of a full `make build-hw` + `make flash-hw`.
#
#   ./deploy_app.sh ui-layer          (make deploy-ui)
#   ./deploy_app.sh backend-daemon    (make deploy-backend)
#
# What it does:
#   1. `bitbake <recipe>` inside the Yocto container: the exact same build
#      (toolchain, flags, sandbox unit) the image would get.
#   2. Copies the binary and its systemd unit(s) from the recipe's install
#      folder (${D}, "image/" in its work folder) onto the board.
#   3. Restarts the service.
#
# Dev only: the change lives on the board's rootfs until the next flash,
# and it needs SSH (so not on the production image). Anything else the
# recipe installs (config files, tmpfiles, ...) is NOT copied -- for those,
# do a normal build-hw + flash-hw.
# =============================================================================
set -euo pipefail

RECIPE="${1:?usage: $0 ui-layer|backend-daemon}"
BOARD_HOST="${BOARD_HOST:-stm32mp1.local}"
cd "$(dirname "$0")"

# The recipe name is also the binary's and the service's name.
case "$RECIPE" in
    ui-layer|backend-daemon) ;;
    *) echo "❌ Unknown app '$RECIPE' (ui-layer or backend-daemon)"; exit 1 ;;
esac

echo "🔨 Building $RECIPE in the Yocto container..."
docker compose up -d >/dev/null
docker compose exec -T yocto-builder bash -c \
    "cd /home/builder/workspace && source poky/oe-init-build-env build >/dev/null && bitbake $RECIPE"

# The recipe's install folder. The glob matches whatever the tune/version
# folder names are, so a future toolchain change doesn't break this.
IMAGE_DIR=$(ls -d build/tmp/work/*/"$RECIPE"/*/image 2>/dev/null | head -1)
if [ -z "$IMAGE_DIR" ] || [ ! -x "$IMAGE_DIR/usr/bin/$RECIPE" ]; then
    echo "❌ No built $RECIPE found under build/tmp/work"
    exit 1
fi

echo "📲 Copying to $BOARD_HOST..."
# To a temporary name first, then renamed in one step on the board: the
# running program's file is never half-written, and a lost connection
# leaves the old version in place.
scp -q "$IMAGE_DIR/usr/bin/$RECIPE" "root@$BOARD_HOST:/usr/bin/.$RECIPE.new"
# The unit files too: #38 changes ui-layer's sandbox (e.g. /dev/dri access).
for unit in "$IMAGE_DIR"/usr/lib/systemd/system/*; do
    [ -f "$unit" ] && scp -q "$unit" "root@$BOARD_HOST:/usr/lib/systemd/system/"
done

echo "🔄 Restarting $RECIPE..."
ssh "root@$BOARD_HOST" "mv /usr/bin/.$RECIPE.new /usr/bin/$RECIPE \
    && systemctl daemon-reload \
    && systemctl restart $RECIPE \
    && sleep 2 \
    && systemctl --no-pager --lines=8 status $RECIPE"
