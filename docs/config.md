# Configuration

**English** · [简体中文](config-cn.md)

`config.toml` holds the policy: how fast the fans should turn at a given
temperature. It contains no hardware knowledge; every pin and sensor name it
references is resolved through [`platform.toml`](platform.md),
case-insensitively.

Related: [design](design.md) · [platform support](platform.md) · [cost](cost.md)

## Shape

```toml
[global]
interval_seconds = 10
min_duty_cycle = 30
max_duty_cycle = 100
default_duty_cycle = 30
fallback_duty_cycle = 60

[[zones]]
name = "cpu"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]

[[zones.thresholds]]
temperature = 50
duty_cycle = 35

[[zones.thresholds]]
temperature = 70
duty_cycle = 60

[[zones.thresholds]]
temperature = 84
duty_cycle = 100
```

A zone needs at least one threshold; the curve is the policy. Everything else is
coupling, limits and failure handling.

Unknown keys are an error, so a misspelled field cannot silently fall back to a
default.

## `[global]`

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| `interval_seconds` | yes | — | Sampling period, ≥ 1. The period includes the cycle's own work: a 10 s period with a 130 ms read sleeps 9.87 s. |
| `min_duty_cycle` | yes | — | Hard floor, 1–100. `1` because a stopped server fan is not a supported state. |
| `max_duty_cycle` | yes | — | Hard ceiling, ≤ 100. |
| `default_duty_cycle` | yes | — | Standby value, used below the first threshold and written back on shutdown. Must not exceed the first step. |
| `fallback_duty_cycle` | yes | — | Value used when a zone cannot read anything. Must be at least the zone's `default_duty_cycle`. |
| `debounce_celsius` | no | `2.0` | Hysteresis for dropping a step. Must be smaller than the gap to the next threshold. |
| `aggregation` | no | `max` | `max` or `mean` across a zone's sources. |
| `fallback_after_cycles` | no | `2` | Consecutive cycles without a reading before a zone drops to `fallback_duty_cycle`. ≥ 1. |
| `command_timeout_seconds` | no | `10` | Deadline for one `ipmitool` invocation. ≥ 1. |

Five keys may be omitted in the whole pair of files: `debounce_celsius`,
`aggregation`, `fallback_after_cycles`, `command_timeout_seconds`, and
`ipmitool_path` in `platform.toml`. Every other key is either required or
inherited from `[global]` into a zone.

## `[[zones]]`

A zone maps a set of sensors to a set of pins.

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| `name` | yes | — | Unique ignoring case. Used in logs, including the cycle line. |
| `monitor_sources` | yes | — | Non-empty. Each entry is a key of `[sensors]`, or `GPU` / `GPU<n>`. |
| `target_pins` | yes | — | Non-empty. Each must be declared in `[pins]`. |
| `thresholds` | yes | — | At least one, strictly ascending by `temperature`. |
| `aggregation` | no | inherits `[global]` | `max` or `mean`, over the sources that reported this cycle. |
| `default_duty_cycle` | no | inherits `[global]` | 1–100, not above the first step. |
| `fallback_duty_cycle` | no | inherits `[global]` | 1–100, not below this zone's `default_duty_cycle`. |
| `fallback_after_cycles` | no | inherits `[global]` | ≥ 1. |
| `debounce_celsius` | no | inherits `[global]` | ≥ 0, smaller than the gap to the next threshold. |

### `[[zones.thresholds]]`

| Key | Required | Meaning |
| --- | --- | --- |
| `temperature` | yes | °C, strictly greater than the previous step. |
| `duty_cycle` | yes | 1–100. Need not ascend, but a descending curve is almost always a mistake. |

A step applies from its temperature up to the next one; above the last step the
last duty cycle holds. Below the first step the zone uses `default_duty_cycle`.

### Inheritance

Five keys are shared policy: `aggregation`, `default_duty_cycle`,
`debounce_celsius`, `fallback_duty_cycle`, `fallback_after_cycles`. Declared in
`[global]` they apply to every zone; declared inside a `[[zones]]` block they
override the global value for that zone.

Inheritance is resolved once at load time. The control loop never consults the
global value.

A coolant pump uses per-zone overrides for both values:

```toml
[global]
default_duty_cycle = 30
fallback_duty_cycle = 60

[[zones]]
name = "pump"
monitor_sources = ["Water_Inlet"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100    # never runs slow
fallback_duty_cycle = 100   # never stops, even when blind

[[zones.thresholds]]
temperature = 20
duty_cycle = 100
```

### GPU sources

`monitor_sources` accepts three kinds of entry:

| Entry | Reads |
| --- | --- |
| `"CPU_Tctl"`, any `[sensors]` key | that BMC sensor |
| `"GPU"` | the hottest of every NVIDIA card NVML can see |
| `"GPU0"`, `"GPU1"`, … | that card, by NVML device index |

`GPU` is reserved; `[sensors]` may not define it. Indices follow the driver's
enumeration order and stay stable across restarts unless cards are moved.
`fanctl check` prints index, name and temperature for every card.

`mean` averages only the sources that reported this cycle, so an absent GPU does
not pull a zone towards 0 °C.

## `[[follows]]`

Coupling between pins, applied after every zone has decided:

```toml
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

This means "the intake is never slower than the radiator". Both ends must be
driven by a zone; `pin` may not follow itself; a cycle of rules is rejected at
load time. Several rules are solved to a fixed point, so the result does not
depend on the order they are written in. See [design](design.md#merging).

## Name matching

Names are compared case-insensitively: `cpu_fan`, `CPU_FAN` and `Cpu_Fan` are the
same pin; `"gpu"` is `"GPU"`, `"gpu1"` is `"GPU1"`.

At load time every reference in `config.toml` is rewritten to the spelling
declared in `platform.toml`, and only then compared exactly, so `check` and
`run` cannot disagree about which pin was meant. Two keys in `platform.toml`
that differ only in case are rejected. SDR names additionally ignore
separators: `CPU_Tctl_Value`, `CPU Tctl Value` and `cpu-tctl-value` are the same
sensor.

## Validation

`fanctl check` and the start of `fanctl run` run the same validator, which
reports every problem at once:

- TOML syntax and unknown keys;
- thresholds strictly ascending; `debounce_celsius` smaller than the gap between
  consecutive thresholds;
- every referenced pin and sensor exists; `[sensors]` does not claim a `GPU`
  name;
- `1 <= min_duty_cycle <= max_duty_cycle <= 100`;
- every zone's `default_duty_cycle` inside `[min, max]` and not above its first
  step;
- every zone's `fallback_duty_cycle` inside `[min, max]` and not below its own
  `default_duty_cycle`;
- `fallback_after_cycles >= 1`, `command_timeout_seconds >= 1`,
  `interval_seconds >= 1`;
- `follows`: both ends driven, no self-reference, no cycle;
- names that collide ignoring case.

Whether `GPU<n>` exists is a runtime fact. `check` prints the cards it found, and
a zone referring to a missing card follows the "no reading" path (hold, then fall
back) instead of failing to start.

## Example

`examples/config.toml` is a complete, commented file to start from. The policy
below runs on the reference machine: a water-cooled EPYC with two 4080s on a
shared radiator and an air-cooled 3080.

```toml
[global]
interval_seconds = 10
min_duty_cycle = 30
max_duty_cycle = 100
default_duty_cycle = 35
fallback_duty_cycle = 60
fallback_after_cycles = 2
debounce_celsius = 2
aggregation = "max"

# Pump: flat out, always - including when nothing can be read, and including
# the shutdown path, where it is written back at 100 % instead of standby.
[[zones]]
name = "pump"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100
fallback_duty_cycle = 100
[[zones.thresholds]]
temperature = 0
duty_cycle = 100

# CPU heatsink and rear exhaust, on package temperature.
[[zones]]
name = "cpu"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
[[zones.thresholds]]
temperature = 45
duty_cycle = 40
[[zones.thresholds]]
temperature = 55
duty_cycle = 50
[[zones.thresholds]]
temperature = 65
duty_cycle = 60
[[zones.thresholds]]
temperature = 72
duty_cycle = 70
[[zones.thresholds]]
temperature = 78
duty_cycle = 80
[[zones.thresholds]]
temperature = 84
duty_cycle = 100

# The radiator over the two 4080s. The 3080 is deliberately absent: it has its
# own fans, and binding this zone to every GPU would run the radiator for heat
# it is not cooling. The curve stops at 70 % because that is the case's
# positive-pressure ceiling, not the radiator's thermal one.
[[zones]]
name = "radiator"
monitor_sources = ["GPU1", "GPU2"]
target_pins = ["SYS_FAN_2"]
default_duty_cycle = 30
[[zones.thresholds]]
temperature = 50
duty_cycle = 35
[[zones.thresholds]]
temperature = 58
duty_cycle = 45
[[zones.thresholds]]
temperature = 68
duty_cycle = 55
[[zones.thresholds]]
temperature = 74
duty_cycle = 60
[[zones.thresholds]]
temperature = 79
duty_cycle = 70

# Front panel: every heat source in the case, including the self-fanned 3080.
# The top radiator pulls case air, so the front fans are the inlet for the whole
# machine; watching only the two 4080 would let the 3080 recirculate its own
# exhaust.
[[zones]]
name = "intake"
monitor_sources = ["CPU_Tctl", "GPU"]
target_pins = ["SYS_FAN_1"]
default_duty_cycle = 35
[[zones.thresholds]]
temperature = 50
duty_cycle = 35
[[zones.thresholds]]
temperature = 58
duty_cycle = 45
[[zones.thresholds]]
temperature = 66
duty_cycle = 60
[[zones.thresholds]]
temperature = 74
duty_cycle = 80
[[zones.thresholds]]
temperature = 80
duty_cycle = 100

# Positive pressure: the intake is never slower than the radiator.
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

Every duty cycle above is a multiple of 5 %. The BMC quantises PWM, so on this
board 25, 28 and 30 % produce the same 828 RPM. Each step was also read back as
RPM after it was applied.
