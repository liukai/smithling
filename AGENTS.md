# Notes for coding agents

Start with [STARTER_GUIDE.md](STARTER_GUIDE.md) (architecture, milestones) and the
[roadmap](specs/000-smithling-master-plan/roadmap.md) (status, decisions, next steps). Each milestone has its own folder
in `specs/` with a spec, design, tasks and a code starter guide.

## Before committing

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd ui && npm test)            # when ui/ changed
```

## Keep the history

These keep [CHANGELOG.md](CHANGELOG.md), the decision log and the tags trustworthy for the next reader. CI fails a
pull request that changes code, UI or specs without touching CHANGELOG.md (`scripts/check-changelog.sh`).

| When | Do |
|---|---|
| a change someone would look up later (behavior, defaults, a new command or view, a deferred item) | one line under **Unreleased** in CHANGELOG.md, with its decision ID if any |
| an earlier decision changes | a new row in [decisions.md](specs/000-smithling-master-plan/decisions.md) and a line in the roadmap's Decisions list; link it from the changelog |
| a milestone closes | rename **Unreleased** to `## Mn: <name> (<date>, tag mn)`, start a new **Unreleased**, tag the closing commit `mn`, update the roadmap's STATE and Progress |

## Never commit

Credentials, `.env` files, real trace data, or personal paths and identities. Recorded SDK fixtures go through
`scripts/record-sdk-fixtures.py` and must not contain local paths.
