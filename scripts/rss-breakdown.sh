#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Attributes fanctl's resident set: program, libc, and the NVIDIA driver.
#
# Same binary, two configs. With no GPU source configured the program never
# calls nvmlInit, so the difference between the two runs is what the driver
# allocates, not what fanctl allocates.
#
# Run on the target machine as a user that may sudo.
set -e
cd "$HOME/Git/fanctl"

BIN=/usr/local/bin/fanctl
[ -x "$BIN" ] || BIN=target/release/fanctl

breakdown() {
    sudo awk '
        /^[0-9a-f]+-[0-9a-f]+ / { path = $6; if (path == "") path = "[anon]" }
        /^Rss:/ { rss[path] += $2; total += $2 }
        END {
            printf "%8d  TOTAL\n", total
            for (p in rss) printf "%8d  %s\n", rss[p], p
        }' "/proc/$1/smaps" | sort -nr | head -8
}

# The child of sudo, not sudo itself: sudo maps libcrypto, PAM and SQLite, which
# is 8 MB of misreading if it is measured by mistake.
child_of() {
    for _ in $(seq 40); do
        kid=$(pgrep -P "$1" | head -1)
        [ -n "$kid" ] && { echo "$kid"; return; }
        sleep 0.1
    done
}

run_for() {
    local config=$1 seconds=$2 label=$3
    sudo "$BIN" --config "$config" --platform /etc/fanctl/platform.toml \
        run --dry-run >"/tmp/rss-$label.log" 2>&1 &
    local parent=$!
    sleep 0.5
    local pid
    pid=$(child_of "$parent")
    sleep "$seconds"
    echo "=== $label ($seconds s, $(grep -c 'cycle temps' "/tmp/rss-$label.log") cycle lines)"
    # PSS divides libc and the binary among every process mapping them; RSS does
    # not, and overstates what this program costs by the size of libc.
    sudo grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem|Private_Dirty):'         "/proc/$pid/smaps_rollup"
    breakdown "$pid"
    sudo kill -TERM "$pid" 2>/dev/null || true
    wait "$parent" 2>/dev/null || true
    echo
}

# A copy of the deployed config with every GPU source dropped.
sudo cp /etc/fanctl/config.toml /tmp/cfg-nogpu.toml
sudo sed -i 's/"GPU[0-9]*"/"CPU_Tctl"/g; s/, *"GPU"//g; s/"GPU", *//g' /tmp/cfg-nogpu.toml
sudo chmod 644 /tmp/cfg-nogpu.toml

run_for /tmp/cfg-nogpu.toml 40 no-gpu-source
run_for /etc/fanctl/config.toml 40 with-gpu-source

echo "=== deployed service, uptime $(ps -o etime= -p "$(systemctl show -p MainPID --value fanctl)" | tr -d ' ')"
D=$(systemctl show -p MainPID --value fanctl)
sudo grep -E '^(Rss|Pss|Pss_Anon|Pss_File|Pss_Shmem|Private_Dirty|Anonymous):'     "/proc/$D/smaps_rollup"
breakdown "$D"
