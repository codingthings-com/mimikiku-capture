#!/usr/bin/env bash
# Build (if needed) and run mimikiku-capture. Settings come from .env in this
# directory (copy .env.example) or MIMIKIKU_* env vars; extra flags such as
# --device or --no-save are passed through and win over both.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release --quiet

exec ./target/release/mimikiku-capture "$@"
