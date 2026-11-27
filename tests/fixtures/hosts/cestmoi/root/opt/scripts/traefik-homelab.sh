#!/usr/bin/env bash
# Keeps dokploy-traefik attached to the "homelab" network.
# Dokploy (re)creates Traefik with only its own network; this reattaches it within ~1s.
attach() {
  if ! docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' dokploy-traefik 2>/dev/null | grep -qw homelab; then
    docker network connect homelab dokploy-traefik 2>/dev/null && echo "attached dokploy-traefik to homelab"
  fi
}
attach
docker events --filter container=dokploy-traefik --filter event=start --format '{{.Action}}' |
  while read -r _; do attach; done
