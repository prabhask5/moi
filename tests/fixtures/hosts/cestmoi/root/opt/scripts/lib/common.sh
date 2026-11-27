#!/bin/bash
log() { echo "$(date -Is) $*" >> /var/log/homelab.log; }
NOTIFY=/opt/scripts/lib/notify.sh
