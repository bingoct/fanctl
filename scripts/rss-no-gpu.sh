#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Measures the resident set of a build without NVML support, so the cost doc can
# say how much of the 28 MB belongs to the NVIDIA driver rather than to fanctl.
# Dry run: it reads the BMC but never writes a PWM value.
set -e
cd "$HOME/Git/fanctl"
cargo build --release --no-default-features --target-dir /tmp/fanctl-nogpu >/dev/null 2>&1

BIN=/tmp/fanctl-nogpu/release/fanctl
stat -c '%s bytes (no nvml)' "$BIN"

sudo "$BIN" --config /etc/fanctl/config.toml --platform /etc/fanctl/platform.toml \
    run --dry-run >/tmp/nogpu.log 2>&1 &
PID=$!
sleep 12
grep -E 'VmRSS|RssAnon|RssFile|Threads' /proc/$PID/status
kill -TERM $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
