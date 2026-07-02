#!/usr/bin/env bash
# Full local verification gate: formatting, lints (zero warnings), tests.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
