# fanctl

**English** · [简体中文](README-cn.md)

A fan control daemon for server boards whose BMC fan control is unusable, or
offers nothing between loud and too hot. It reads temperatures from the BMC,
converts them to duty cycles through a per-zone curve, and writes the result
back to the BMC.

- Temperatures come from BMC sensors (IPMI raw) and NVIDIA GPUs (NVML).
- The control period is set by `interval_seconds`; the reference configuration
  uses 10 s. All reads of a cycle share one `ipmitool exec`, all writes share
  another.
- Pin ids, sensor numbers, command templates and the tachometer scale live in
  `platform.toml`. There is no hexadecimal constant in the Rust source.

The reference deployment is a TYAN S8030GM2NE (EPYC 7413, 3 GPUs), running as a
systemd service. Porting to another board means writing a `platform.toml`, not
changing code.

## Features

- **Temperature-driven control.** One step curve per zone. A rising temperature
  jumps immediately, possibly several steps; a falling one must clear
  `debounce_celsius` before dropping a step.
- **Board decoupled.** `platform.toml` describes the hardware, `config.toml`
  describes the policy; the two are validated as a pair.
- **Zones.** A zone takes any number of inputs: one BMC sensor, a single GPU, or
  the hottest of all GPUs.
- **Coupling.** A pin referenced by several zones receives the highest duty
  cycle; `[[follows]]` adds fixed constraints between pins.
- **IPMI raw collection.** No `ipmitool sdr` on the hot path. Only the sensors a
  zone references are read, and seven readings share one `exec` in 183 ms.
- **Failure policy.** A zone with no reading holds its previous step, and drops
  to `fallback_duty_cycle` after `fallback_after_cycles` consecutive misses. A
  PWM write without a BMC acknowledgement is logged and repeated next cycle.
- **One Rust binary.** 1.44 MiB, statically linked apart from libc and libgcc_s,
  2 threads, 8 MB resident without NVML loaded.

## Supported platforms

| Board | Status | Notes |
| --- | --- | --- |
| TYAN S8030GM2NE | verified on hardware | 4 PWM channels, sensor numbers 0x60–0x64, tachometer scale 138 RPM/count |
| other boards with an OEM raw fan command | usable, needs a config | command template and sensor numbers differ per vendor |

The hardware facts a `platform.toml` needs can only be measured. PWM channel
numbers and sensor numbers are two unrelated numbering spaces, and the mapping
from channel to physical header has to be confirmed by watching fan speeds.
[docs/platform.md](docs/platform.md) documents the fields, how to look the
numbers up, and how to add a board. Before the first write, `fanctl check`
cross-checks every number in the file against `ipmitool sdr`.

## Requirements

- Linux with systemd.
- `ipmitool` ≥ 1.8 with access to `/dev/ipmi0` (root, or a udev rule granting
  `CAP_SYS_RAWIO`).
- A BMC that accepts a raw PWM command.
- Rust 1.84 or later to build (`resolver = "3"`). `Cargo.lock` is committed.
- Optional: the NVIDIA driver and NVML for GPU temperatures. Build with
  `--no-default-features` on a host without it; the binary is 106 KB smaller.

## Install

No packages yet. Build and install by hand:

```bash
cargo build --release                              # GPU temperatures wanted
cargo build --release --no-default-features        # no NVIDIA driver

sudo install -Dm755 target/release/fanctl    /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml   /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml     /etc/fanctl/config.toml

sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
```

`examples/` holds the reference configuration for the S8030. For another board,
rewrite `platform.toml` following [docs/platform.md](docs/platform.md), then
adjust `config.toml` following [docs/config.md](docs/config.md).

## Use

```bash
# Validate both files and probe the hardware: check sensor numbers, cross-check
# raw readings, list the GPUs, do one batched read. Writes no PWM.
sudo fanctl check

# Run the full control loop without sending PWM commands. The log carries what
# each cycle would have written.
sudo fanctl run --dry-run -v

# Run in the foreground, supervised by systemd.
sudo fanctl run
```

One log line per cycle, rate limited by changes in temperature, fan speed and
sensor presence. `-v` prints every cycle, `-vv` adds TRACE. Exit code is 0 on a
clean shutdown, 1 on a configuration or hardware error. The line format, the
rate limits and the shutdown behaviour are in
[docs/design.md](docs/design.md#per-cycle-log).

Export the temperature history:

```bash
journalctl -u fanctl -o cat | grep '^cycle ' > cycles.log
```

## Configuration

| File | Content | Documentation |
| --- | --- | --- |
| `/etc/fanctl/platform.toml` | hardware: pin ids, sensor numbers, command template, tachometer scale | [docs/platform.md](docs/platform.md) |
| `/etc/fanctl/config.toml` | policy: zones, curves, coupling, fallbacks | [docs/config.md](docs/config.md) |

Both files are validated at startup and by `fanctl check`; unknown keys are an
error. Names are compared case-insensitively.

## Documentation

| Document | Content |
| --- | --- |
| [docs/design.md](docs/design.md) | the cycle, the read protocol, aggregation, curve and debounce, merging, failure policy, logging, safety limits |
| [docs/config.md](docs/config.md) | every `config.toml` field with defaults, validation rules, a complete example |
| [docs/platform.md](docs/platform.md) | `platform.toml` fields, finding sensor numbers and PWM channels, adding a board |
| [docs/cost.md](docs/cost.md) | measured read/write times, binary composition, resident memory, comparison with a similar implementation |

## Development

Three checks have to pass on Linux before a commit (`util/process.rs` handles
timeouts and signals with `cfg(unix)`, so `cargo test` on Windows is not
meaningful):

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npx markdownlint-cli2 "**/*.md"     # documentation changes
```

Hardware constants belong in `platform.toml`, not in `src/`. A new GPU backend
plugs into the `Source` enum in `src/sensors.rs`; a board that publishes GPU
temperatures as IPMI sensors needs no code at all, only `[sensors]` entries.
When reporting a control problem, include the `fanctl check` output and a few
`cycle` lines.

Most of the code was written by an AI agent; the design was set by the
maintainer and every hardware fact was measured on the machine: which channel
drives which header, the fan speed at each duty step, that `raw 0x04 0x2d`
returns a tachometer count rather than RPM, where the coolant pump saturates.
That data is in the configuration comments and in these documents. 124 tests
pass, `clippy -D warnings` and `rustfmt` are clean. Operational experience is
limited to one board, so a first deployment on another board should follow
`check` → `--dry-run` → supervised run.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
