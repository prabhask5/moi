#!/usr/bin/env python3
"""Keep local settings in Dokploy's traefik.yml if Dokploy rewrites it:
  - 1h read timeout on both entry points (large uploads)
  - TLS on for every router on :443 (Dokploy relies on this), using the default
    cert (Cloudflare Origin CA) instead of a Let's Encrypt resolver
Restarts Traefik only when something changed; prints a message only then."""
import subprocess, yaml

PATH = "/etc/dokploy/traefik/traefik.yml"
READ_TIMEOUT = "1h"

with open(PATH) as f:
    cfg = yaml.safe_load(f) or {}
before = yaml.safe_dump(cfg, sort_keys=False)

eps = cfg.setdefault("entryPoints", {})
for name in ("web", "websecure"):
    ep = eps.get(name)
    if isinstance(ep, dict):
        rt = ep.setdefault("transport", {}).setdefault("respondingTimeouts", {})
        rt["readTimeout"] = READ_TIMEOUT

ws = eps.get("websecure")
if isinstance(ws, dict):
    http = ws.get("http") if isinstance(ws.get("http"), dict) else {}
    tls = http.get("tls") if isinstance(http.get("tls"), dict) else {}
    tls.pop("certResolver", None)
    http["tls"] = tls
    ws["http"] = http

after = yaml.safe_dump(cfg, sort_keys=False)
if after != before:
    with open(PATH, "w") as f:
        f.write(after)
    subprocess.run(["docker", "restart", "dokploy-traefik"], check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print("traefik.yml drift fixed and Traefik restarted")
