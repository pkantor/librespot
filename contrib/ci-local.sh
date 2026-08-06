#!/usr/bin/env bash
#
# Runs what .github/workflows/pipeline.yml runs, so a push does not fail on something
# reproducible here. From the repo root:
#
#   $ ./contrib/ci-local.sh
#
# The Pi's feature set includes alsa-backend, which only builds on Linux. On other hosts that
# part is delegated to `cross`, which needs Docker running; without it the script says so and
# skips it, and those lints stay unverified until CI.

set -euo pipefail

FEATURES=rustls-tls-native-roots,alsa-backend,with-libmdns
export RUSTFLAGS=${RUSTFLAGS:--D warnings}

echo "==> cargo fmt"
cargo fmt --all -- --check

echo "==> cargo clippy (default features)"
cargo clippy --workspace --all-targets

echo "==> cargo test"
cargo test --workspace --locked

echo "==> cargo clippy ($FEATURES)"
if [ "$(uname -s)" = "Linux" ]; then
  cargo clippy -p librespot --all-targets --no-default-features --features "$FEATURES"
elif docker info >/dev/null 2>&1; then
  # the same toolchain and target the release build uses, so the cfg-gated code is really compiled
  cargo install --locked cross --quiet 2>/dev/null || true
  cross clippy -p librespot --all-targets --target aarch64-unknown-linux-gnu \
    --no-default-features --features "$FEATURES"
else
  echo "    SKIPPED: needs Linux or a running Docker daemon for cross."
  echo "    Code behind #[cfg(feature = \"alsa-backend\")] is not linted by this run."
  exit 1
fi
