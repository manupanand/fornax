#!/usr/bin/env bash
# Copyright 2026 Manu P Anand
# Licensed under the Apache License, Version 2.0
# See LICENSE for details.
#
# Build the fornax-gcp .deb and .rpm (one static musl binary for every supported distro).
# Runs on the build server, not the laptop:
#   scripts/remote-build.sh --dir fornax-dev -- scripts/package-fornax-gcp.sh
# Output: target/packages/fornax-gcp_<ver>_amd64.deb, fornax-gcp-<ver>.x86_64.rpm, SHA256SUMS
#
# Needs: rustup + cargo, cc, strip, dpkg-deb (for cargo-deb). No rpmbuild, no sudo.
# Env: FORNAX_GCP_TARGET=x86_64-unknown-linux-gnu builds against glibc instead of musl
#      (the binary then needs the build host's glibc or newer; see docs/fornax-gcp-install.md).
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_DEB_VERSION="3.8.0"
CARGO_GENERATE_RPM_VERSION="0.21.1"
target="${FORNAX_GCP_TARGET:-x86_64-unknown-linux-musl}"
crate=crates/fornax-gcp
out=target/packages

need_tool() { # <cargo subcommand> <crate> <version>
  if ! cargo "$1" --version 2>/dev/null | grep -qF "$3"; then
    echo "==> installing $2 $3 (user-level)"
    cargo install --locked "$2" --version "$3"
  fi
}
need_tool deb cargo-deb "$CARGO_DEB_VERSION"
need_tool generate-rpm cargo-generate-rpm "$CARGO_GENERATE_RPM_VERSION"

if ! rustup target list --installed | grep -qx "$target"; then
  rustup target add "$target"
fi

# Release builds use the committed Cargo.lock. If it is not up to date yet (work in progress),
# warn and let cargo resolve; FORNAX_GCP_LOCKED=1 makes that an error instead.
locked=(--locked)
if ! cargo metadata --locked --offline --format-version 1 >/dev/null 2>&1   && ! cargo metadata --locked --format-version 1 >/dev/null 2>&1; then
  if [[ "${FORNAX_GCP_LOCKED:-0}" == 1 ]]; then
    echo "error: Cargo.lock is out of date (FORNAX_GCP_LOCKED=1)" >&2
    exit 1
  fi
  echo "warning: Cargo.lock is out of date; building without --locked" >&2
  locked=()
fi

echo "==> building fornax-gcp ($target, release)"
cargo build --release "${locked[@]}" -p fornax-gcp --target "$target"
bin="target/$target/release/fornax-gcp"
strip -s "$bin"
file "$bin" 2>/dev/null || true
if [[ "$target" == *musl* ]] && ldd "$bin" 2>&1 | grep -q '=>'; then
  echo "error: $bin is not statically linked" >&2
  exit 1
fi
version="$("$bin" --version | awk '{print $2}')"

# The packaged example must stay what `fornax-gcp config example` prints.
if ! diff <("$bin" config example) <(grep -v '^#' "$crate/packaging/config.example.toml" | sed '/./,$!d'); then
  echo "error: $crate/packaging/config.example.toml differs from 'fornax-gcp config example'" >&2
  exit 1
fi

rm -rf "$out"
mkdir -p "$out"
deb="$out/fornax-gcp_${version}_amd64.deb"
rpm="$out/fornax-gcp-${version}.x86_64.rpm"

echo "==> .deb"
# --no-build: package the binary built (and stripped) above; --no-strip: already stripped.
cargo deb -p fornax-gcp --target "$target" --no-build --no-strip --output "$deb"

echo "==> .rpm"
cargo generate-rpm -p "$crate" --target "$target" --output "$rpm"

(cd "$out" && sha256sum -- *.deb *.rpm >SHA256SUMS)
echo "==> done"
ls -l "$out"
cat "$out/SHA256SUMS"
