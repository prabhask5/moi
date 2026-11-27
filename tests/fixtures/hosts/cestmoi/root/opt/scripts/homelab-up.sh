#!/bin/bash
# start every compose stack under /opt/services
. /opt/scripts/lib/common.sh
for d in /opt/services/*/; do (cd "$d" && docker compose up -d); done
