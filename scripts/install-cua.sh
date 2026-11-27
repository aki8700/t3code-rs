#!/usr/bin/env bash
# Build, sign, and install the bundled SwiftCUA (macOS Computer Use) app and LaunchAgent.
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../swift-cua"
exec ./scripts/install.sh "$@"
