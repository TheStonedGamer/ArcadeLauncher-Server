#!/usr/bin/env bash
set -euo pipefail

apt-get update
apt-get install -y curl build-essential pkg-config cmake ca-certificates

if [[ ! -x /root/.cargo/bin/cargo ]]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

/root/.cargo/bin/rustc --version
/root/.cargo/bin/cargo --version
