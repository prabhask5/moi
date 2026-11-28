# Roadmap

What moi does today is in the [README](../README.md) and, in full, in the [spec](spec/README.md); what changed between releases is in the [changelog](../CHANGELOG.md). This file is only about what is ahead. Work is tracked as [issues](https://github.com/prabhask5/moi/issues).

## Before 1.0

Every command is implemented, and tested in CI at three levels: against fixture servers, against real services (Uptime Kuma 1.x and 2.x, Postgres, an S3 server, restic), and on real virtual machines (backup, rebuild on a second machine, reboot, verify; a migration on real systemd and cron; the firewall's rollback timer; packages on Ubuntu, Debian and Fedora; SSH with the released client and OpenSSH).

What no test can stand in for comes first:

- **The first real server.** cestmoi, the homelab moi was written for, moves from its own scripts to moi with `moi migrate`: shadow for a week, compare, switch, finish. What that turns up is fixed before 1.0.
- **`moi ssh up` against a real Cloudflare account**, and **`moi devbox up` on real hardware**: both are tested against fakes of the other side (and the real `cloudflared`), not against the services themselves.
- **1.0.0**: the release after that, with the crate on crates.io and the Homebrew formula in the tap.

## Later

- OpenRC and other init systems (systemd only today).
- Two-factor authentication for the Uptime Kuma user moi logs in as.
- More than one off-site repository.

## Not planned

Multi-server orchestration, a web UI, Kubernetes, Windows or macOS servers, a restic replacement, managing settings inside applications.
