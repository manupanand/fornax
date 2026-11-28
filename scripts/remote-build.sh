#!/usr/bin/env bash
# Copyright 2026 Manu P Anand
# Licensed under the Apache License, Version 2.0
# See LICENSE for details.
#
# Sync this working tree to the build server and run a command there (we do not build on
# laptops: Rust targets are large). Usage:
#   scripts/remote-build.sh [--dir NAME] -- cargo test -p fornax-gcp
# Defaults: host ansiblenode, dir ~/DevOps/code-pe/fornax-dev (NOT the git clone).
set -euo pipefail
cd "$(dirname "$0")/.."
host="${FORNAX_BUILD_HOST:-ansiblenode}"
dir="DevOps/code-pe/fornax-dev"
if [[ "${1:-}" == "--dir" ]]; then dir="DevOps/code-pe/$2"; shift 2; fi
[[ "${1:-}" == "--" ]] && shift
ssh -o BatchMode=yes "$host" "mkdir -p ~/$dir"
tar --exclude=./target --exclude=./web/node_modules --exclude=./data --exclude=./.git -cf - . \
  | ssh -o BatchMode=yes "$host" "cd ~/$dir && tar -xf - --overwrite"
ssh -o BatchMode=yes "$host" "cd ~/$dir && source ~/.cargo/env && $*"
