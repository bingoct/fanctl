# fanctl

**English** · [简体中文](README-cn.md)

When a server board's BIOS does not regulate fan speed by temperature, fanctl
takes over: it reads temperatures, converts them to duty cycles through
per-zone curves, and writes the result back to the BMC over IPMI raw.

## Features

- **Temperature control.** One step curve per zone. A rise jumps steps
  immediately, possibly several at once; a fall drops one step only after
  crossing `debounce_celsius`. [Design](docs/design.md#the-curve)
- **Board decoupled.** Hardware in `platform.toml`, policy in `config.toml`,
  validated as a pair. [Platform](docs/platform.md) ·
  [Configuration](docs/config.md)
- **Zones.** A zone references any number of inputs: one BMC sensor, one GPU,
  the maximum over all GPUs. [Design](docs/design.md#aggregation)
- **Coupling.** A pin shared by several zones takes the maximum duty cycle;
  `[[follows]]` expresses fixed constraints between pins.
  [Design](docs/design.md#merging)
- **IPMI raw.** Only the sensors a zone references are read, one `ipmitool`
  process per cycle. [Design](docs/design.md#reading)
- **Failure policy.** A zone with no reading holds its current step, then drops
  to `fallback_duty_cycle`; a write the BMC did not acknowledge is resent next
  cycle. [Design](docs/design.md#when-a-reading-disappears)
- **Tachometer readback.** Fan speeds are logged and a stopped fan raises an
  alarm; speeds do not feed the curve. [Design](docs/design.md#per-cycle-log)

## Verified boards

| Board | Notes |
| --- | --- |
| TYAN S8030GM2NE | 4 PWM channels, sensor numbers 0x60–0x64, tach scale 138 RPM/count |

A new board needs its own `platform.toml`: field reference, how to find sensor
numbers, and how to add a board are in
[docs/platform.md](docs/platform.md).

## Requirements

- Linux with systemd.
- `ipmitool` ≥ 1.8, able to open `/dev/ipmi0` (root, or a udev rule granting
  `CAP_SYS_RAWIO`).
- A BMC that exposes writable raw PWM commands.
- Rust 1.84 or later to build (the lower bound for `resolver = "3"`).
  `Cargo.lock` is committed.
- GPU temperatures need the NVIDIA driver and NVML; on a host without the
  driver, build with `--no-default-features`.

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

`examples/` is the reference configuration for the S8030. On a different
board, write your own `platform.toml` first
(see [docs/platform.md](docs/platform.md)), then edit `config.toml`; run
the last two systemd steps only after that.

## Use

Before starting the service, read the [Configuration](#configuration)
section: zones, curves, and fallback duty cycles live in `config.toml`,
and the examples in this repository describe one machine, the S8030.

```bash
# Validate both configurations and probe the hardware: check sensor numbers,
# cross-validate raw readings, list the GPUs, do one batched read.
# Writes no PWM.
sudo fanctl check

# Run the full control loop without sending PWM commands.
sudo fanctl run --dry-run -v

# Run in the foreground, supervised by systemd.
sudo fanctl run
```

A clean exit returns 0; a configuration or hardware error returns 1. The log
format, its rate limits and the shutdown reset are in
[docs/design.md](docs/design.md#per-cycle-log).

## Configuration

| File | Content | Documentation |
| --- | --- | --- |
| `/etc/fanctl/platform.toml` | hardware: pin ids, sensor numbers, command template, tach scale | [docs/platform.md](docs/platform.md) |
| `/etc/fanctl/config.toml` | policy: zones, curves, coupling, fallback | [docs/config.md](docs/config.md) |

Both files are validated at startup and by `fanctl check`; unknown fields are
an error. Name matching is case-insensitive.

## Documentation

| Document | Content |
| --- | --- |
| [docs/design.md](docs/design.md) | cycle flow, read/write protocol, aggregation, curve and hysteresis, merging, failure policy, logging, safety limits |
| [docs/config.md](docs/config.md) | every `config.toml` field with defaults, validation rules, a full example |
| [docs/platform.md](docs/platform.md) | `platform.toml` fields, finding sensor numbers and PWM channels, adding a board |
| [docs/cost.md](docs/cost.md) | measured read/write times, binary composition, resident memory, comparison with similar implementations |

## Development

Most of the code was written by AI agents, checked by the test suite and
reviewed by the maintainer.

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npx markdownlint-cli2 "**/*.md"
```

Hardware constants live only in `platform.toml`, never in `src/`. A new GPU
backend goes into the `Source` enum in `src/sensors.rs`; once GPU temperatures
are published as IPMI sensors, a new board needs only `[sensors]` entries, no
code.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
