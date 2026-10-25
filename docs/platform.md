# Platform support

**English** · [简体中文](platform-cn.md)

`platform.toml` describes the machine. It is the only file that knows a
hexadecimal number, and the only file that has to be replaced to run fanctl on
a different board. `config.toml` — the policy — stays as it is.

Related: [configuration](config.md) · [design](design.md) · [cost](cost.md)

## Verified boards

| Board | BMC | Status | Details |
|---|---|---|---|
| TYAN S8030GM2NE | AST2500 | verified on hardware | 4 PWM channels in use, sensors 0x60–0x64, tachometer scale 138 RPM/count |
| any board with an OEM raw fan command | — | expected to work | needs a new `platform.toml`; see [adding a board](#adding-a-board) |

"Verified" means: `fanctl check` passes against the live BMC, every sensor
number was cross-checked against `ipmitool sdr get`, the channel-to-header
mapping was confirmed by writing one channel and watching which fan changed
speed, and the daemon has run as a systemd service.

Not supported, and not close to working: Dell iDRAC, HPE iLO and Supermicro
boards with Redfish fan profiles. They expose fan control over a web API rather
than a raw IPMI command, which needs a different actuator backend. The
interface for that is small (see [contributing](../README.md#contributing)) but
nobody has written it.

## The file

```toml
[platform]
name = "TYAN S8030GM2NE"
cmd_template = "raw 0x2e 0x44 0xfd 0x19 0x00 {id} 0x01 {pwm}"
rpm_per_count = 138

[pins]
"CPU_FAN"   = "0x00"
"SYS_FAN_1" = "0x02"
"SYS_FAN_2" = "0x03"
"SYS_FAN_4" = "0x05"

[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }

[tachometers]
"CPU_FAN" = { sdr = "CPU_FAN", id = "0x60" }
```

### `[platform]`

| Key | Required | Default | Meaning |
|---|---|---|---|
| `name` | yes | — | Board name. Logs only. |
| `cmd_template` | yes | — | `ipmitool` arguments, must contain `{id}` and `{pwm}`. `{pwm}` renders as `0xNN`. |
| `ipmitool_path` | no | `ipmitool` (via `$PATH`) | Use an absolute path under systemd, where `$PATH` is minimal. |
| `rpm_per_count` | when `[tachometers]` is not empty | — | RPM per raw tachometer count. See [tachometers](#tachometers). |

`cmd_template` is the vendor's private command. It is expanded once per pin per
write; nothing else in the program knows what it means.

### `[pins]`

Logical channel name → channel id, both board-specific.

| | |
|---|---|
| Required | at least one |
| Values | `0x00`–`0xFF`, quoted |
| Names | unique ignoring case; used by `config.toml` |

The name is yours to choose (`"CPU_FAN"`, `"front"`, `"pump"`), and it is the
only handle `config.toml` uses. A pin that no zone targets is never written and
never restored on exit; `check` reports it as a `[warn]`, which is expected if
you keep a channel declared only for its tachometer.

### `[sensors]`

Logical sensor name → how to read it. Both halves are required and both are
checked:

```toml
[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
```

- `id` is the IPMI **sensor number**, used by the control loop
  (`raw 0x04 0x2d <id>`).
- `sdr` is the name `ipmitool sdr` prints. `check` uses it for diagnostics, for
  cross-validation of `id`, and for the human-readable failure messages.

Two names may not share a number: one of them would silently read the other's
temperature. `GPU`, `GPU0`, `GPU1` … are reserved and rejected here — they are
NVIDIA devices, not BMC sensors (see [configuration](config.md#gpu-sources)).

Only sensors some zone references are ever read. Declaring one costs nothing.

### `[tachometers]`

Logical pin name → the fan-speed sensor on the same header. Optional.

```toml
[platform]
rpm_per_count = 138

[tachometers]
"SYS_FAN_1" = { sdr = "SYS_FAN_1", id = "0x61" }
```

Fan speeds are reported in the cycle log line and warn when a fan stops
turning. They never influence a duty cycle.

Two things are easy to get wrong here:

**The number is not the channel.** `SYS_FAN_1` may be written through channel
`0x02` and read back from sensor `0x61`. The PWM channel and the tachometer
sensor are different numbering spaces with no arithmetic relation; the only way
to find one is to look it up.

**`raw 0x04 0x2d` returns a count, not RPM.** `ipmitool sdr` prints RPM because
it applies the M coefficient from the SDR record locally; the BMC does not.
On the S8030 every fan reads a multiple of 138, so `rpm_per_count = 138`
turns count 7 into 966 RPM. `check` compares `count × rpm_per_count` against
`ipmitool sdr get` and fails if they differ by more than one count — a wrong
coefficient would print a confident, plausible number for a fan nobody is
measuring.

The key must name a pin declared in `[pins]`. If the hardware changes so the
channel no longer drives anything — a coolant pump wired straight to a Molex
plug, which is a reasonable thing to do — keep the pin declared and delete the
zone instead: the tachometer keeps working, and the channel is simply never
written.

## Finding the numbers

Everything `platform.toml` needs is on the machine already.

**Sensor names and numbers:**

```bash
sudo ipmitool sdr | grep -i -E 'fan|temp'
sudo ipmitool sdr get CPU_Tctl_Value | head -3     # Sensor ID : CPU_Tctl_Value (0x1)
```

**PWM channels** are not discoverable — the mapping from channel number to
physical header is vendor knowledge. If the board has a vendor tool that sets
fan speed manually, use it and read the channels back; otherwise find the
command in the vendor's documentation or a forum thread for that board, then
prove the mapping by hand:

```bash
# Write one channel, watch which fan changes, then put the value back.
sudo ipmitool raw 0x2e 0x44 0xfd 0x19 0x00 0x03 0x01 0x64    # channel 3 -> 100 %
sleep 12
sudo ipmitool sdr type Fan
```

A proper test uses two very different duty cycles, not two neighbouring ones:
fan readings are quantised (138 RPM per count here), so 30 % and 35 % can
return the same number through no fault of the hardware. On the S8030 the
coolant pump reads 1104 at 30 % and 3036 at 100 %, which is unambiguous.

**Tachometer scale:** compare one raw read with one `sdr get`:

```bash
sudo ipmitool raw 0x04 0x2d 0x61     # 07 c0 c0 00
sudo ipmitool sdr get SYS_FAN_1      # Sensor Reading : 966 (+/- 0) RPM
```

`966 / 7 = 138`. Then `fanctl check` will keep you honest from then on.

## Adding a board

1. Copy `examples/platform.toml` and replace `name`, `cmd_template`, `[pins]`,
   `[sensors]`.
2. Run `sudo fanctl check`. It prints, per sensor, whether the number exists,
   whether `sdr` resolves to that number, and whether a raw read agrees with
   `ipmitool sdr`; then it lists the GPUs; then it does one batched read of
   everything and reports the timing.
3. Add `[tachometers]` and `rpm_per_count` if you want speeds in the log.
4. Run `fanctl run --dry-run -v` and compare the logged duty cycles with what
   you expect from a hand-written curve.
5. Only then run for real, watching `journalctl -u fanctl -f` for the first few
   minutes — while confirming that the header that changed is the one you
   intended.

If the board needs a command shape `cmd_template` cannot express — a
per-channel checksum, a two-step unlock before each write — the actuator needs
code rather than configuration. Open an issue with the command sequence; the
rendering is one function (`PwmActuator::render`).

A `platform.toml` that works, plus a `config.toml` that demonstrates it, is the
contribution this project needs most.
