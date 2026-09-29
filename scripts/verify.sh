#!/usr/bin/env bash
# The one verification command. Run it on Linux with the Windows GNU std installed
# (the Eureka verify image has both); a Windows workstation proves nothing.
# The Windows check runs first: it is fast, and a hung test cannot mask it.
# --lib because the tests under tests/ assume the host layout.
set -euo pipefail
cargo check --locked --target x86_64-pc-windows-gnu --bin idunn-host
cargo test --locked --lib
