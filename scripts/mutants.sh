#!/usr/bin/env bash
# Mutation testing in an isolated Podman container. Mutated code is
# untrusted by construction, so the repository is mounted read-only and
# all builds/tests run on a container-local copy; only mutants.out/
# (gitignored) receives results.
#
# Slow (minutes for a full sweep) — run manually after adding rules or
# refactoring match/apply logic. Not part of check.sh or pre-commit.
#
# Usage: scripts/mutants.sh [cargo-mutants args...]
#   e.g. scripts/mutants.sh --list
#        scripts/mutants.sh -f src/dropbox.rs
# Exit codes (cargo-mutants): 0 clean, 2 missed mutants, 3 timeouts,
# 4 baseline tests already failing.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if ! command -v podman >/dev/null; then
    echo "error: podman is required for isolated mutation runs" >&2
    exit 1
fi

image=localhost/dropignore-mutants
podman build --quiet -f scripts/mutants.Containerfile -t "$image" scripts >/dev/null

# mutants.out/ is the mount; results appear in mutants.out/mutants.out/ so
# cargo-mutants can rotate the previous run without crossing the mount.
mkdir -p mutants.out
exec podman run --rm \
    --volume "$PWD":/src:ro \
    --volume "$PWD/mutants.out":/out \
    --volume dropignore-mutants-cargo:/usr/local/cargo/registry \
    --workdir /src \
    "$image" cargo mutants -o /out "$@"
