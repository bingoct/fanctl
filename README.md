# fanctl

**English** · [简体中文](README-cn.md)

A fan control daemon for server boards whose BMC has no usable fan curve.

It reads temperatures straight off the BMC over IPMI — and NVIDIA GPUs over
NVML — turns them into duty cycles with a per-zone curve, and writes the result
back with `ipmitool raw`. Built for a Tyan S8030 whose "Smart Fan" is either a
jet engine or a slow cook, but nothing in the control path knows about that
board: every hex constant, sensor number and command template lives in
`platform.toml`.

```
cycle temps CPU_Tctl=23.0 GPU0=28.0 GPU1=26.0 | fan CPU_FAN=690 SYS_FAN_1=966 | duty cpu=30 intake=35
```

## Features

**Dynamic control, not a static profile.** A five-step curve per zone with
asymmetric hysteresis: a rising temperature switches immediately (possibly
several steps), a falling one has to clear the step below by
`debounce_celsius` before dropping. Heat gets a reaction, noise gets a delay.
See [design](docs/design.md#the-curve).

**The board is a configuration file.** Pin ids, sensor numbers, the `ipmitool`
argument template and the tachometer scale all live in `platform.toml`. There
is not one hexadecimal constant in the Rust source. Another board means another
file, not another fork — see [platform support](docs/platform.md).

**Zones.** A zone is one question ("how fast should these fans turn?") answered
from any number of sensors: CPU, case air, individual GPU, mean over a group.
One pin can be claimed by several zones and always gets the highest demand, and
`[[follows]]` expresses plain couplings ("the intake must never be slower than
the CPU fan") without a tangle of conditionals. See
[design](docs/design.md#merging).

**IPMI raw, not `ipmitool sdr`.** The whole SDR table costs 2.9 s per poll
because every row is a round trip. fanctl reads only the sensors a zone
actually uses, packs every read into a single `ipmitool exec`, and parses the
BMC's own replies: 94 ms for one sensor, 185 ms for seven. Writes are batched
the same way — four pins in 135 ms instead of 419 ms. See
[cost](docs/cost.md).

**Rust, one small binary.** 1.44 MiB, statically linked apart from libc and
libgcc_s, no runtime, 2 threads, 8 MB resident without the NVIDIA driver
loaded. `panic = "abort"`, `#[forbid(unsafe_code)]`, no `unwrap` in production
code.

Status: 124 tests, clean `clippy -D warnings` and `rustfmt`, all on Linux. The
acceptance run on a TYAN S8030GM2NE (EPYC 7413, 3 GPUs) covered `check` against
the real BMC, a 5-minute dry run, ten minutes of live control and a `SIGTERM`
restore; the daemon has been running there as a systemd service since. Read
[Safety](docs/design.md#safety-and-limits) before pointing it at a machine you
care about.

## Supported platforms

| Board | Status | Notes |
|---|---|---|
| TYAN S8030GM2NE | verified | CPU_FAN + 3 × SYS_FAN headers, tachometer scale 138 RPM/count, `raw 0x2e 0x44 0xfd 0x19 0x00 <ch> 0x01 <duty>` |
| other boards with an OEM raw fan command | should work | needs a `platform.toml`; the command template and sensor numbers differ per vendor |

The daemon speaks IPMI generically — what it cannot guess is the vendor's
private PWM command. If your board answers to `raw 0x2e 0x44 …` (most
AST2500/AST2600 boards with a "Smart Fan" and a manual override do) it is a
configuration exercise, and [docs/platform.md](docs/platform.md) walks through
it: finding sensor numbers, mapping channels, and closing the loop to prove
which header is which. `fanctl check` verifies the mapping against the live BMC
before the first write.

## Requirements

- Linux with systemd. Nothing else in the way of init integration is provided.
- `ipmitool` ≥ 1.8, with permission to reach `/dev/ipmi0` (root, or a udev rule
  granting `CAP_SYS_RAWIO`).
- A BMC whose fan channels accept a raw PWM command.
- Rust 1.84+ to build (`resolver = "3"` needs it). `Cargo.lock` is committed.
- Optional: the NVIDIA driver, if you want GPU temperatures. Build with
  `--no-default-features` on a machine without one.

## Install

There are no packages yet. Build and install by hand:

```bash
cargo build --release
cargo build --release --no-default-features    # no NVIDIA driver on the host

sudo install -Dm755 target/release/fanctl          /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml         /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml           /etc/fanctl/config.toml
```

`examples/` carries the reference configuration for the verified board — start
there, then read [configuration](docs/config.md) and adjust the curve to your
hardware. Both files are validated as a pair; `fanctl check` reports every
problem at once instead of the first one.

## Use

Three steps, in this order. Each one is a superset of the previous.

```bash
# 1. Validate both config files and probe the hardware: reads the sensor
#    numbers named in platform.toml, cross-checks them against `ipmitool sdr`,
#    lists the GPUs, and does one batched raw read. Writes nothing.
sudo fanctl check

# 2. Run the control loop but send no PWM command. Logs exactly what it would
#    have written, every cycle. This is the step that catches "the curve is
#    wrong", not the config validator.
sudo fanctl run --dry-run -v

# 3. Run for real, in the foreground, and let systemd supervise it.
sudo fanctl run
```

Under systemd:

```bash
sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
journalctl -u fanctl -f
```

The unit is written for root and for a foreground process: no `PIDFile=`, no
`PrivateDevices=` (it would hide `/dev/ipmi*`), `TimeoutStopSec=120s` to cover
the restore writes.

Logs are the interface. One line per cycle, rate-limited by change:

| what happened | level |
|---|---|
| a temperature moved ≥ 3 °C, a fan speed moved ≥ 2 tach counts, or a sensor appeared/vanished | INFO |
| nothing moved, less than 5 minutes since the last INFO line | DEBUG |
| nothing moved, but 5 minutes passed | INFO |
| a driven fan stopped reporting rotation | WARN |

So an idle box writes 288 lines a day, a busy one writes a line every 10 s, and
a dead fan is never silent. `-v` raises the level to DEBUG (every cycle), `-vv`
to TRACE. Exit code is 0 on a clean shutdown and 1 on a configuration or
hardware error.

Draw a temperature curve:

```bash
journalctl -u fanctl -o cat | grep '^cycle ' > cycles.log
```

## Configuration

Two files, deliberately separate:

- `platform.toml` — what the hardware is: pins, sensor numbers, how to talk to
  it. → [docs/platform.md](docs/platform.md)
- `config.toml` — what the policy is: zones, curves, coupling, fallbacks.
  → [docs/config.md](docs/config.md)

Both are validated at startup and by `fanctl check`; unknown keys are an error,
not a silently ignored typo. Names are case-insensitive everywhere.

## Design

The interesting parts — aggregation, the curve, merging several zones onto one
pin, what happens when a sensor dies — are in [docs/design.md](docs/design.md).
The measured cost of a cycle and where the binary's bytes go are in
[docs/cost.md](docs/cost.md).

## Contributing

The most useful contribution is **another board**. `docs/platform.md` documents
every field and the procedure for finding the numbers; a pull request that adds
a working `platform.toml` (plus a config demonstrating it) turns this from a
one-board tool into a general one. Boards whose fan control is not a raw OEM
command — Dell iDRAC, HPE iLO, Supermicro with a Redfish fan profile — would
need a different actuator backend, which is a small, well-isolated module.

The second most useful contribution is **another GPU backend**. Temperatures
currently come from NVML, which covers NVIDIA only. AMD (ROCm SMI) and Intel
(`xe`/`i915` hwmon) both expose per-card temperatures through other interfaces,
and a board that publishes GPU temperatures as IPMI sensors needs no new code
at all — just `[sensors]` entries. The `Source` enum in `src/sensors.rs` is
where a new backend plugs in.

Before sending a patch: `cargo fmt --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test`. Keep board-specific knowledge out of `src/` — if a
constant is hardware, it belongs in `platform.toml`. Include the output of
`fanctl check` and a few `cycle` lines when reporting a control problem; the
whole point of that log line is to make a remote diagnosis possible.

## How this was built

Written with an AI agent doing most of the typing, under a maintainer who
specified the design and verified every hardware claim on the machine itself:
which header is which channel, at what duty a fan actually starts turning, that
`raw 0x04 0x2d` returns a tachometer count rather than RPM, that a coolant pump
saturates at 90 %. Those facts are in the comments and in these documents
because they were measured, not because they were assumed — several of the
first assumptions were wrong and were corrected against the hardware.

What that means for you: the code is small (about 5 000 lines, of which the
control path is a few hundred), the tests are cheap to run without any
hardware, and no part of it is magic. It also means the project has exactly one
maintainer's worth of operational experience, on exactly one board. Treat a
first deployment the way it was treated here — `check`, then `--dry-run`, then
watch it.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
