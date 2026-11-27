#!/bin/bash
# third hop from a unit: still captured
curl -fsS -m 10 "$1" >/dev/null
