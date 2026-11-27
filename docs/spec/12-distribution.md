# 12 — Distribution & releases

One static binary per architecture; every channel repackages the same GitHub Release artifacts.

| Channel | How | Notes |
|---|---|---|
| GitHub Releases | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` tarballs + `SHA256SUMS` + build provenance attestation | source of truth |
| Installer | `curl -fsSL https://raw.githubusercontent.com/prabhask5/moi/main/install.sh \| sh` | detects arch, verifies checksum, installs to `/usr/local/bin/moi`; `--version X`; installs `restic` from the distro if missing (asks) |
| `.deb` / `.rpm` | `cargo-deb`, `cargo-generate-rpm`, attached to releases | installs `/usr/bin/moi`, the unit file (disabled until `moi init`), shell completions, man page; apt repo later |
| crates.io | package `cestmoi`, binary `moi`: `cargo install cestmoi` / `cargo binstall cestmoi` | |
| Homebrew | tap `prabhask5/homebrew-tap`, formula `moi` | brew refuses to run as root; `moi init` prints the `sudo` command to install the system service |
| AUR / Nix | later | |

## Versioning & release flow

- SemVer. `CHANGELOG.md` (Keep a Changelog). Breaking: config keys, Kuma message formats, state layout, CLI flags.
- Tag `vX.Y.Z` on `main` → `release.yml`: build matrix (cargo-zigbuild for musl), strip, package tar/deb/rpm, checksums, attestations, create the GitHub Release with the changelog section, publish to crates.io (`CARGO_REGISTRY_TOKEN`), update the Homebrew tap formula (`HOMEBREW_TAP_TOKEN`).
- `moi --version` prints version, git sha, build date, target.
- The state dir and `moi.db` carry a schema version; moi migrates them forward on start and refuses to run against a newer schema.

## Acceptance

- [ ] `release.yml` dry-run on a `v0.0.0-test` tag in a fork produces all artifacts.
- [ ] Installer verifies the checksum and fails closed on mismatch.
- [ ] The deb installs on Ubuntu 24.04/26.04 and Debian 13 containers; the rpm on Fedora; `moi --version` runs on all.
