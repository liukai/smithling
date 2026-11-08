#!/usr/bin/env bash
# Fails when code, UI or specs changed since BASE but CHANGELOG.md didn't. CI runs it on every pull request;
# run it yourself before opening one:  scripts/check-changelog.sh master
set -euo pipefail
base="${1:?usage: scripts/check-changelog.sh <base ref, e.g. master or origin/main>}"
changed=$(git diff --name-only "$base"...HEAD)
if ! grep -qE '^(crates/|ui/src/|specs/|\.github/|Cargo\.toml)' <<<"$changed"; then
  echo "no code, UI or spec changes since $base: no changelog entry needed"
  exit 0
fi
if grep -qx 'CHANGELOG.md' <<<"$changed"; then
  echo "CHANGELOG.md updated"
  exit 0
fi
echo "Code, UI or specs changed since $base, but CHANGELOG.md didn't."
echo "Add a line under 'Unreleased' (see the top of CHANGELOG.md for what belongs there),"
echo "or label the pull request 'skip-changelog' if nobody would ever look this change up."
exit 1
