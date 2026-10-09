#!/usr/bin/env bash
# Run cargo inside the pinned Rust image. Use this when the host toolchain cannot
# link (e.g. macOS with an unaccepted Xcode licence) or to reproduce CI exactly.
#
#   scripts/cargo-docker.sh test -p dc3-bencode
#
# DC3_TARGET   target directory name under ./target (default: docker). Parallel
#              callers should use distinct names to avoid waiting on cargo's lock.
# DC3_NETWORK  docker network to join (default: dc3-dev, if it exists), so tests
#              can reach the dev PostgreSQL container at dc3-test-pg:5432.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Pinned like the Dockerfile's RUST_IMAGE: the tag alone floats.
image="rust:1.99-slim-trixie@sha256:24e632c09342c20abf8312cf4f61430a911c01ed3a5e4c02b87292b1c39c5273"
target="${DC3_TARGET:-docker}"
network="${DC3_NETWORK:-dc3-dev}"

net_args=()
if docker network inspect "$network" >/dev/null 2>&1; then
  net_args=(--network "$network")
fi

env_args=()
for var in DATABASE_URL DC3_TEST_DATABASE_URL PROPTEST_CASES RUST_BACKTRACE DC3_E2E_VERBOSE; do
  if [[ -n "${!var:-}" ]]; then
    env_args+=(-e "$var=${!var}")
  fi
done

exec docker run --rm \
  ${net_args[@]+"${net_args[@]}"} \
  ${env_args[@]+"${env_args[@]}"} \
  -v "$root":/w \
  -v dc3-cargo-registry:/usr/local/cargo/registry \
  -v dc3-cargo-git:/usr/local/cargo/git \
  -e CARGO_TARGET_DIR="/w/target/$target" \
  -e CARGO_TERM_COLOR=never \
  -w /w \
  "$image" \
  cargo "$@"
