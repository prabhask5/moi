# Working on moi with an AI agent

moi is built by an AI coding agent (Claude Code), reviewed by the owner. This file is the contract.

## Every session

1. Read `CLAUDE.md`, `docs/roadmap.md`, and the spec files the issue you work on names. The legacy scripts whose behaviour checks must reproduce are in the cestmoi fixture (`tests/fixtures/hosts/cestmoi/root/opt/scripts/`).
2. Work on **one issue per branch**, named for what it does.
3. Tests first: turn the spec's acceptance criteria into failing tests in `tests/`, then implement until they pass.
4. Run `scripts/ci.sh` before every commit; never push red.
5. If the spec is wrong, ambiguous or missing something: decide, then update the spec (and `CLAUDE.md` if architecture changes) **in the same PR**, and say so in the PR description.
6. **Cleanup pass before the PR**: delete what the change superseded, debug code, scratch/spike files, unused fixtures and dependencies; no TODO stays; `scripts/check-hygiene.sh` green. The repo must be publishable after every PR.
7. Open a PR: what changed, which acceptance criteria are now covered, anything deferred (as an issue), spec changes.

## Hard rules

- Never run moi, tests or scripts against the real system of the machine you are on (no real `/etc`, Docker socket, `systemctl`, `iptables`). Use `FixtureHost` and the fixtures. Integration tests run in CI containers.
- Never weaken a parity test to make it pass; parity output is a contract (05).
- Never add secrets to the repo; fixtures use `FAKE…` tokens. The fixture server is redacted — keep it that way.
- Never add a dependency without a line in `docs/spec/14-security.md#dependencies`.
- Keep `README.md` user-facing and current for shipped commands only.
- Leave nothing behind: no commented-out code, no `*.bak`/scratch dirs, no notes files, no unexplained `#[ignore]`.

## Kickoff prompt (paste into Claude Code at the repo root)

> Read CLAUDE.md, docs/roadmap.md and docs/agent-workflow.md. Then take the issue I name (or the first open one). Before writing code, list the acceptance criteria you will turn into tests and any spec ambiguities with the decision you propose. Then implement on a branch, keep `scripts/ci.sh` green, and open a PR.

## Slash command

`.claude/commands/next-milestone.md` contains the kickoff prompt so it can be run as `/next-milestone`.
