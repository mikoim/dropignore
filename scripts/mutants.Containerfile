# Image for containerized mutation testing (see scripts/mutants.sh).
# rustc matches the host toolchain; cargo-mutants is pinned so runs are
# reproducible. Both are baked into a cached layer because
# `cargo install cargo-mutants` costs minutes.
FROM docker.io/library/rust:1.96-slim
RUN cargo install cargo-mutants --version 27.1.0 --locked
