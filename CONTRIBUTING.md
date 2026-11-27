# Contributing

Thanks for helping. moi runs as root on people's servers, so correctness and safety come first.

1. Read [CLAUDE.md](CLAUDE.md) and the relevant [spec](docs/spec/README.md). Behaviour changes start as spec changes.
2. Pick an open milestone issue or discuss a new idea in an issue first.
3. Tests first, in `tests/`, against fixture hosts — never against your real system.
4. `scripts/ci.sh` must be green. Keep PRs focused; describe which acceptance criteria they cover.
5. Never commit secrets; `scripts/check-secrets.sh` runs in CI.

Health-check messages and `--json` output are contracts: changing them is a breaking change and needs a CHANGELOG entry.
