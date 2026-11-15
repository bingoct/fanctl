# Platform

**English** · [简体中文](platform-cn.md)

`platform.toml` describes the hardware: it is the only file containing
hexadecimal numbers, and the only file to replace when the board changes.
`config.toml` stays as it is.

Related: [configuration](config.md) · [design](design.md) · [cost](cost.md)

## Verified boards

| Board | BMC | Details |
| --- | --- | --- |
| TYAN S8030GM2NE | AST2500 | 4 PWM channels, sensors 0x60–0x64, tachometer scale 138 RPM/count |

"Verified" means: `fanctl check` passes against the live BMC; every sensor
number was cross-checked against `ipmitool sdr get`; the channel-to-header
mapping was confirmed by writing a single channel and watching which fan
changed speed; and the daemon has run as a systemd service.

Dell iDRAC, HPE iLO, and Supermicro boards using Redfish fan profiles are not
supported: they control fans through a web API rather than raw IPMI, which
requires a different actuator backend. The backend interface is small
(`PwmActuator`), and no implementation exists yet.

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
| --- | --- | --- | --- |
| `name` | yes | — | Board name, used in logs. |
| `cmd_template` | yes | — | `ipmitool` arguments, must contain `{id}` and `{pwm}`; `{pwm}` renders as `0xNN`. |
| `ipmitool_path` | no | looked up from `$PATH` | Under systemd `$PATH` is short, so an absolute path is advisable. |
| `rpm_per_count` | required when `[tachometers]` is not empty | — | RPM per tachometer count, see [tachometers](#tachometers). |

`cmd_template` is the vendor's private command. It is expanded once per pin
per write, and nothing else in the program interprets it.

### `[pins]`

Logical channel name to channel id; both are determined by the board.

| | |
| --- | --- |
| Required | at least one |
| Values | `0x00`–`0xFF`, quoted |
| Names | unique ignoring case, referenced by `config.toml` |

The name is arbitrary (`"CPU_FAN"`, `"front"`, `"pump"`) and is the only
handle `config.toml` uses. A pin that no zone targets is never written and
never restored on exit; `check` reports it as a `[warn]`, which is the
expected case for a channel declared only to read its tachometer.

### `[sensors]`

Logical sensor name to how to read it. Both parts are required and both are
validated:

```toml
[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
```

- `id` is the IPMI sensor number, used by the control loop
  (`raw 0x04 0x2d <id>`).
- `sdr` is the name in `ipmitool sdr` output. `check` uses it for
  diagnostics, to cross-validate `id`, and to generate readable error
  messages.

Two names must not share one number — one of them would silently read the
other's temperature. `GPU`, `GPU0`, `GPU1` … are reserved and rejected here:
they are NVIDIA devices, not BMC sensors, see
[configuration](config.md#gpu-sources).

Only sensors referenced by a zone are read; declaring an unused sensor costs
nothing.

### Tachometers

Logical pin name to the fan-speed sensor on the same header. Optional.

```toml
[platform]
rpm_per_count = 138

[tachometers]
"SYS_FAN_1" = { sdr = "SYS_FAN_1", id = "0x61" }
```

Fan speeds appear in the cycle log line, and a stopped fan produces a
warning; speeds never take part in duty cycle computation.

**The sensor number is not the channel.** `SYS_FAN_1` may be written through
channel `0x02` and read back from sensor `0x61`. PWM channels and sensor
numbers are two numbering spaces with no conversion between them; each has
to be looked up individually.

**`raw 0x04 0x2d` returns a count, not RPM.** `ipmitool sdr` prints RPM
because it applies the M coefficient from the SDR record locally; the BMC
does not. On the S8030 every fan reading is a multiple of 138, so
`rpm_per_count = 138` turns count 7 into 966 RPM. `check` compares
`count × rpm_per_count` against `ipmitool sdr get` and fails beyond a
three-count difference.

The key must name a pin declared in `[pins]`. When a hardware change leaves
the channel no longer driving a fan — a coolant pump rewired to a direct
Molex feed, for example — keep the pin declared and delete the zone: the
tachometer keeps working and the channel is never written again.

## Finding the numbers

Everything `platform.toml` needs is available on the target machine.

**Sensor names and numbers:**

```bash
sudo ipmitool sdr | grep -i -E 'fan|temp'
sudo ipmitool sdr get CPU_Tctl_Value | head -3     # Sensor ID : CPU_Tctl_Value (0x1)
```

**PWM channels cannot be discovered automatically.** The mapping from channel
number to physical fan header is vendor knowledge. If the board has a vendor
tool that can set fan speed manually, use it to set a speed and read the
channel back; otherwise find the command in the vendor documentation or a
forum thread for that board, then prove the mapping by hand:

```bash
# Write one channel, watch which fan changes, then put the value back.
sudo ipmitool raw 0x2e 0x44 0xfd 0x19 0x00 0x03 0x01 0x64    # channel 3 -> 100 %
sleep 12
sudo ipmitool sdr type Fan
```

The two duty cycles being compared must be far apart: fan readings are
quantised (138 RPM per count on this board), so 30 % and 35 % can return the
same number. On the S8030 the pump reads 1104 at 30 % and 3036 at 100 %.

**Tachometer scale:** compare one raw read with one `sdr get`:

```bash
sudo ipmitool raw 0x04 0x2d 0x61     # 07 c0 c0 00
sudo ipmitool sdr get SYS_FAN_1      # Sensor Reading : 966 (+/- 0) RPM
```

`966 / 7 = 138`. Every later `fanctl check` re-verifies this value.

## Adding a board

1. Copy `examples/platform.toml` and replace `name`, `cmd_template`,
   `[pins]` and `[sensors]`.
2. Run `sudo fanctl check`. For each sensor it reports whether the number
   exists, whether `sdr` points at that number, and whether the raw reading
   agrees with `ipmitool sdr`; then it lists the GPUs and performs one
   batched read with timing.
3. Add `[tachometers]` and `rpm_per_count` to get fan speeds in the log.
4. Run `fanctl run --dry-run -v` and compare the logged duty cycles against
   the curve.
5. Only then run for real, watching `journalctl -u fanctl -f` for the first
   few minutes to confirm that the header changing is the intended one.

When the command form a board needs cannot be expressed by `cmd_template` —
a per-channel checksum, a two-step unlock before each write — the execution
layer needs code, not configuration. Open an issue with the command
sequence; the rendering logic lives in one place, `PwmActuator::render`.
