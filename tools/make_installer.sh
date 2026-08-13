#!/usr/bin/env bash
# Build the release exe and package the NSIS installer into dist/.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 tools/gen_icon.py
cargo build --release --target x86_64-pc-windows-gnu
mkdir -p dist
makensis installer/resmon.nsi
ls -la dist/ResourceMonitorSetup.exe
