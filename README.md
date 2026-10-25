# fanctl

**English** · [简体中文](README-cn.md)

Temperature-driven fan control for server boards whose BMC fan policy cannot be
used. Reads temperatures, converts them to duty cycles through per-zone curves,
writes the result back over IPMI raw.

## Features

- **Temperature-driven control.** One step curve per zone. A rising temperature
  jumps immediately, possibly several steps; a falling one waits for
  `debounce_celsius`. [Design](docs/design.md#the-curve)
- **Board decoupled.** Hardware in `platform.toml`, policy in `config.toml`,
  validated as a pair. [Platform](docs/platform.md) ·
  [Configuration](docs/config.md)
- **Zones.** Any number of inputs per zone: one BMC sensor, a single GPU, or the
  hottest of all GPUs. [Design](docs/design.md#aggregation)
- **Coupling.** A pin shared by several zones takes the highest duty cycle;
  `[[follows]]` adds fixed constraints between pins.
  [Design](docs/design.md#merging)
- **IPMI raw.** Only the sensors a zone names are read, in one `ipmitool`
  process per cycle. [Design](docs/design.md#reading)
- **Failure policy.** A zone with no reading holds its step, then drops to its
  fallback; a write the BMC did not acknowledge is retried.
  [Design](docs/design.md#when-a-reading-disappears)
- **Tachometer readback.** Speeds are logged and a fan that stops raises a
  warning; the curve does not read them. [Design](docs/design.md#per-cycle-log)

## Verified boards

| Board | Notes |
| --- | --- |
| TYAN S8030GM2NE | 4 PWM channels, sensor numbers 0x60–0x64, tachometer scale 138 RPM/count |

A different board needs its own `platform.toml`. [docs/platform.md](docs/platform.md)
documents the fields, how to find the numbers, and how to add one.

## Requirements

- Linux with systemd.
- `ipmitool` ≥ 1.8 with access to `/dev/ipmi0` (root, or a udev rule granting
  `CAP_SYS_RAWIO`).
- A BMC that accepts a raw PWM command.
- Rust 1.84 or later to build (`resolver = "3"`). `Cargo.lock` is committed.
- Optional: the NVIDIA driver and NVML for GPU temperatures. Build with
  `--no-default-features` on a host without it.

## Install

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

`examples/` holds the reference configuration for the S8030.

## Use

```bash
# Validate both files and probe the hardware: check sensor numbers,
# cross-check raw readings, list the GPUs, do one batched read. Writes no PWM.
sudo fanctl check

# Run the control loop without sending PWM commands.
sudo fanctl run --dry-run -v

# Run in the foreground, supervised by systemd.
sudo fanctl run
```

Exit code is 0 on a clean shutdown, 1 on a configuration or hardware error. The
log line, its rate limits and the shutdown behaviour are in
[docs/design.md](docs/design.md#per-cycle-log).

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
| [docs/cost.md](docs/cost.md) | measured read and write times, binary composition, resident memory, comparison with a similar implementation |

## Development

Most of the code was written by an AI agent, checked by the test suite and
reviewed by the maintainer.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npx markdownlint-cli2 "**/*.md"
```

Hardware constants belong in `platform.toml`, not in `src/`. A new GPU backend
plugs into the `Source` enum in `src/sensors.rs`; a board that publishes GPU
temperatures as IPMI sensors needs no code, only `[sensors]` entries.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
