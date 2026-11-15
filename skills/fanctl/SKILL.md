---
name: fanctl
description: Build, install, configure, and operate the fanctl IPMI fan control daemon, and run this repository's development checks. Use when working on fanctl code, its two configuration files, deployment on a server board, or BMC read/write debugging.
---

# fanctl

Temperature-driven fan control for server boards whose BIOS does not regulate
fan speed by temperature. Reads temperatures (IPMI raw sensors + NVIDIA GPUs
via NVML), maps them to duty cycles through per-zone curves, writes the result
back over IPMI raw. Linux with systemd only.

## Where things are

| Path | Role |
| --- | --- |
| `README.md` | facade: what it is, requirements, install, the three commands |
| `docs/design.md` | the control cycle, read/write protocol, failure policy, logging |
| `docs/config.md` | every `config.toml` field, defaults, validation rules |
| `docs/platform.md` | every `platform.toml` field, finding board numbers, adding a board |
| `docs/cost.md` | measured timings, binary composition, resident memory |
| `examples/` | reference configuration for the TYAN S8030GM2NE |
| `packaging/fanctl.service` | systemd unit |

## Build and development checks

- Rust 1.84+ (the manifest uses `resolver = "3"`); `Cargo.lock` is committed
  because this is a binary crate and deployments must be reproducible.
- GPU support is a cargo feature: `default = ["gpu"]`. Build with
  `--no-default-features` on hosts without the NVIDIA driver.
- A change is done when all of these pass:

  ```bash
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  npx markdownlint-cli2 "**/*.md"
  ```

- `unsafe_code` is forbidden. Hardware constants belong in `platform.toml`,
  never in `src/`. The crate has no `mod.rs` files; do not add them.
- Tests assert against the files in `examples/`, so editing them requires
  running the test suite.
- The target OS is Linux. A `cargo test` on Windows is a smoke check, not
  acceptance; the release build and live hardware checks run on a Linux host.

## Install on a target

```bash
cargo build --release
sudo install -Dm755 target/release/fanctl    /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml   /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml     /etc/fanctl/config.toml
sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
```

Requires `ipmitool` >= 1.8 with access to `/dev/ipmi0` (root, or a udev rule
granting `CAP_SYS_RAWIO`). `examples/` describes the S8030 only: on a
different board, write a board-specific `platform.toml` before the
`systemctl enable --now` step.

## Operating order

1. `sudo fanctl check` — validates both files, verifies every sensor number
   against the live BMC by name, cross-checks one raw read against
   `ipmitool sdr` (within 2 °C), lists the GPUs, does one batched read.
   Writes no PWM.
2. `sudo fanctl run --dry-run -v` — the full loop, logging what it would write.
3. `sudo fanctl run` — foreground, supervised by systemd. Exit 0 on a clean
   shutdown, 1 on a configuration or hardware error. Logs go to journald:
   `journalctl -u fanctl -f`.

Never skip step 1: a wrong sensor number does not error, it silently reads a
different sensor.

## Configuration semantics (the parts that bite)

- Two files, validated as a pair at startup; unknown keys are an error.
- `default_duty_cycle` is standby (used below the first step, written back on
  shutdown); `fallback_duty_cycle` is the no-reading state. Per zone,
  `fallback >= default` is enforced.
- Names are compared case-insensitively. `GPU` is a reserved source name
  (the hottest NVIDIA card); `GPU<n>` is one card by NVML index.
- Only five keys are optional in the whole pair of files:
  `debounce_celsius`, `aggregation`, `fallback_after_cycles`,
  `command_timeout_seconds`, `ipmitool_path`.
- No hot reload: editing configuration means restarting the service.

## Hardware facts (S8030, the verified board)

- PWM channel ids and sensor numbers are separate numbering spaces with no
  arithmetic relation. The only confirmation of a channel is a closed loop:
  write it, watch which tachometer changes speed, restore the value.
- Raw tachometer counts are multiples of 138 RPM on this board
  (`rpm_per_count = 138`).
- A write succeeded when the BMC replies `fd 19 00` (three bytes).
  `ipmitool`'s exit code cannot say which pin failed.
- Reads and writes go through one `ipmitool exec` per cycle, with a separator
  command (`raw 0x06 0x01`) before each reading. Do not simplify the separator
  protocol: it keeps one failed command from shifting every following reading
  onto the wrong name.
