#!/usr/bin/env bash
#
# Pulls the latest aarch64 build of the API branch onto the Raspberry Pi.
# Every build gets its own tag, but /releases/latest/download/ always redirects to the newest
# one, so this URL never changes.
#
# /usr/bin/librespot is the path in the systemd unit's ExecStart, and the restart is what
# actually puts the new binary on the air — without it the running process keeps the old one.

set -eu

curl -fsSL -o /tmp/librespot https://github.com/pkantor/librespot/releases/latest/download/librespot-aarch64
sudo install -m 755 /tmp/librespot /usr/bin/librespot
sudo systemctl restart librespot
