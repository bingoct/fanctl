# Design

**English** · [简体中文](design-cn.md)

One control cycle does five things. Configuration fields are in
[config.md](config.md); measured timings are in [cost.md](cost.md).

## The cycle

A single-threaded loop, no async runtime:

```text
   ┌──────────────────────────────────────────────────────────────┐
   │ 1  read      every sensor a zone references, in one ipmitool │
   │              exec: temperatures from the BMC, GPUs from NVML │
   ├──────────────────────────────────────────────────────────────┤
   │ 2  aggregate per zone: max or mean over the sources that     │
   │              reported this cycle                             │
   ├──────────────────────────────────────────────────────────────┤
   │ 3  curve     temperature -> duty cycle by table lookup,      │
   │              with hysteresis on the way down only            │
   ├──────────────────────────────────────────────────────────────┤
   │ 4  merge     one pin, several zones -> the maximum; then     │
   │              [[follows]]; then clamp to [min, max]           │
   ├──────────────────────────────────────────────────────────────┤
   │ 5  write     only pins whose value changed, in one ipmitool  │
   │              exec, acknowledged per pin by the BMC           │
   └──────────────────────────────────────────────────────────────┘
                        sleep until the next cycle
```

Steps 2 to 5 are pure functions. They touch no BMC, GPU, or board, so the
control logic is testable offline: `tests/control_logic.rs` plus unit tests in
each module.

The period covers the work of the cycle. A 10 s period with a 130 ms read
sleeps 9.87 s. A cycle that overruns the period is logged and the next one
starts immediately; no offset accumulates.

The configuration is read once at startup. Changing it means a restart, so a
half-valid file can never replace a running configuration.

## Reading

`ipmitool sdr` walks the whole sensor table, one round trip per row: 2.9 s on
the S8030. fanctl requests only the sensors its zones reference
(`raw 0x04 0x2d <number>`) and feeds every command through stdin to a single
`ipmitool exec`.

`exec` does not stop at a failing command: it skips the output and continues.
If readings were matched by line, one failed sensor would shift every
following reading onto the wrong name. Each read is therefore preceded by a
separator command with a fixed output length (`raw 0x06 0x01`), and the reply
is split into slots:

```text
raw 0x06 0x01            <- 15 bytes: slot starts
raw 0x04 0x2d 0x01       <- reading for CPU_Tctl
raw 0x06 0x01            <- next slot
raw 0x04 0x2d 0x20       <- no output: this sensor failed
raw 0x06 0x01
raw 0x04 0x2d 0x31       <- reading, still on the right sensor
```

An empty slot means that one command failed. The write path uses the same
protocol, where an empty slot means the pin was not acknowledged (see
[cost](cost.md#writes)).

The first byte of a raw reply is the raw value, which is a temperature or a
tachometer count depending on the sensor. fanctl does not apply the SDR
record's M/B coefficients. `check` verifies that assumption on the actual
board: the raw value multiplied by the scale must agree with `ipmitool sdr`
within 2 °C, otherwise it fails. A reply of "no reading", `ns`, or `disabled`
is treated as missing, never as 0 °C.

## Aggregation

Each zone reduces all of its inputs to one temperature per cycle.

- `max` (default): the zone protects its hottest input, so one hot GPU is not
  diluted by the others.
- `mean`: for inputs that share the same airflow, such as a radiator or a
  drive bank.

Only inputs that reported valid data this cycle are counted: the mean of one
live and one dead sensor is the live sensor's temperature. A zone with no
valid input falls under the [failure policy](#when-a-reading-disappears).

`GPU` is a keyword: the highest temperature among all GPUs visible to NVML.
`GPU1` is a single card by NVML index. Two zones can watch different subsets
of the same hardware:

```toml
# The radiator covers these two cards only.
[[zones]]
name = "radiator"
monitor_sources = ["GPU1", "GPU2"]

# The intake feeds the whole case, self-fanned cards included.
[[zones]]
name = "intake"
monitor_sources = ["CPU_Tctl", "GPU"]
```

A self-fanned card exhausts its heat into the case, and that heat never passes
through the radiator. Referencing `GPU` in the radiator zone would speed the
radiator for heat it does not cool; referencing only `GPU1`/`GPU2` in the
intake zone would under-feed the case.

## The curve

`thresholds` are steps: once the temperature reaches a step's `temperature`,
use that step's `duty_cycle` until the next step. Above the last step the last
step holds; below the first step the zone uses `default_duty_cycle`.

The two directions are asymmetric:

| Direction | Condition | Action |
| --- | --- | --- |
| up | `temperature >= step.temperature` | switch immediately, possibly several steps |
| down | `temperature <= current step.temperature - debounce_celsius` | drop one step |
| below the first step | `temperature <= first.temperature - debounce_celsius` | back to `default_duty_cycle` |
| neither | — | hold |

With steps at 55/70/85 → 40/60/100 % and `debounce_celsius = 2`, 70 °C gives
60 %, and the fan returns to 40 % only at 68 °C. A rise to 71 °C on the way
down snaps back to 60 % immediately, with no hysteresis.

No moving average: it delays the rising edge too, and the rising edge is the
direction that must respond promptly when load appears. The hysteresis state
is one integer; no history buffer is needed.

`debounce_celsius` must be smaller than the gap between adjacent steps,
otherwise a step could never be exited; the validator rejects such a
configuration.

A fan's physical floor affects curve design: below its minimum speed, a lower
duty cycle changes nothing. On the S8030, the stock fans all read 828 RPM at
25 %, 28 %, and 30 %. A step written below that floor is not a configuration
error; it is a fan that cannot go slower.

## Merging

Zones are evaluated independently, so their results can conflict. The
resolution order is fixed:

1. **One pin, several zones: the maximum duty cycle.** Any other rule would
   let a zone evaluated later reduce the cooling an earlier zone requested.
2. **`[[follows]]` iterated to a fixed point.** `pin = A, min_of = B` means
   `A = max(A, B)`. Rules are applied repeatedly until nothing changes, so the
   result does not depend on the order they were written in. A cycle of rules,
   a self-reference, or an end that no zone drives is rejected at load time.
3. **Clamp to `[min_duty_cycle, max_duty_cycle]`**, last, so neither a zone
   nor a coupling rule can exceed the global bounds.
4. **Write only what changed**, compared against the last confirmed value, so
   the BMC is never resent a value it already holds.

`[[follows]]` expresses fixed constraints between pins that are not a curve,
such as "the intake is never slower than the radiator". Encoding that as a
second curve on the same pin would conflict with the first.

## When a reading disappears

Three failures, three behaviours.

**A zone has no reading this cycle.** It holds its previous step, so a single
missed reading produces no audible speed change. After
`fallback_after_cycles` consecutive cycles without data (default 2, 20 s at a
10 s period) the zone drops to `fallback_duty_cycle` and logs it. A single
failed `ipmitool exec` is normal on a busy BMC; the grace period exists for
exactly that.

**No sensor can be read at all.** Skip the grace period and write the fallback
values immediately. When no load is visible, holding an old value is worse
than a known-safe value.

**A write was not acknowledged.** The pin keeps its old value in the "last
sent" table and is resent next cycle. An unacknowledged write is never
recorded as successful.

Two rules make the fallback value dependable:

- `fallback_duty_cycle >= default_duty_cycle`, enforced per zone: with no
  reading at all, the machine is never in its quietest state.
- A zone may override both values. A water pump uses
  `default_duty_cycle = fallback_duty_cycle = 100`, which holds at 100 at
  idle, with no reading, and on exit.

### Shutdown

On `SIGINT` / `SIGTERM`, every driven pin is written back once to its zone's
`default_duty_cycle` and the process returns 0. The BMC keeps the last value
it was written; without this step the machine would stay at the speed the last
load demanded.

Pins that no zone drives are never written, during the loop or on exit.
`check` reports them as `[warn]`; a channel declared only to read its
tachometer is exactly this case.

## Per-cycle log

One line per cycle:

```text
cycle temps CPU_Tctl=23.0 GPU0=28.0 GPU1=26.0 | fan CPU_FAN=690 SYS_FAN_1=966 | duty cpu=30 intake=35
```

Three groups of `key=value` pairs, each introduced by a separator word. Names
are sorted, because the readings live in a hash map and an unstable order
would make two identical cycles look different.

The line is machine-parsable (it is the data source for temperature plots) and
routine evidence that the loop is still running. Emitting it every cycle is
about 8 600 lines a day, so one INFO goes out every five minutes and the rest
drop to DEBUG:

| Event | Level |
| --- | --- |
| 5 minutes since the last cycle line | INFO |
| under 5 minutes, nothing else happening | DEBUG |
| a duty cycle changed | INFO, with the temperatures attached |
| a driven fan went from turning to zero | WARN |

Only these events decide the level. An earlier version also promoted the line
when any temperature moved 3 °C, to record transients between heartbeats. On
an idle machine that threshold sits exactly on the flicker of `CPU_Tctl`
(wandering between 21 and 24 °C): in one day, 189 of 295 intervals were
temperature triggers, 113 of them that flicker, and the real heartbeat got
through only 60 times. A temperature move is worth logging only when it moves
a fan, and that case has a line of its own:

```text
duty cycle changed pin=SYS_FAN_2 from=30 to=35 temps=CPU_Tctl:26.0,GPU0:55.0,GPU1:26.0
```

The cause lands with the effect, so a five-minute sample never has to explain
an adjustment made four minutes earlier.

The first write of each pin after startup is logged as `duty cycle
initialised`, with no temperature: it was not triggered, the daemon is simply
settling the curve on a pin whose previous value it never saw.

Fan speed is in the line but not in the level decision. A tachometer reading
flickers by a count or two on its own (the pump at 100 % alternates between
5934 and 6072 RPM, one count apart); a speed that moved while the temperature
did not does not get its own line. A fan reaching zero does: the duty cycle is
the request that was sent, the speed is what the fan actually did, and a
stalled rotor, a loose cable, or a lost supply rail is visible only in the
speed. That line is emitted on the transition, not repeated every cycle.

## Safety and limits

What the software checks:

- **`check` before `run`.** Validates both configuration files, verifies every
  sensor number by name, cross-validates one raw reading against
  `ipmitool sdr` (2 °C tolerance), lists the GPUs, and performs one batched
  read. It writes no PWM.
- **`--dry-run` before a real run.** Runs the full loop and logs the values it
  would have written.
- **A duty cycle of 0 is rejected.** `min_duty_cycle >= 1`; stopping a server
  fan is not a state this program can express.

What it cannot guarantee:

- **Channel numbers are checked only syntactically.** Only an actual write
  proves which fan header a channel drives. `check` infers the association
  from names and warns when it cannot.
- **No in-process watchdog.** After the process exits, the BMC keeps the last
  written values; restarting is systemd's job. A daemon stopped for a long
  time leaves the fans at their last duty cycle; a clean exit leaves them at
  the standby value.
- **The BMC can take control back.** A reboot, a power cycle, or a switch to
  "Smart Fan" discards the raw writes. The daemon rewrites them on the next
  cycle, so a freshly rebooted machine spends a few seconds under the vendor
  policy.

## Non-goals

- **No hot reload.** A restart is cheap and atomic; a partially applied
  configuration is neither.
- **No state saved on disk.** The step is re-derived from the current
  temperature at startup. The rising direction is undamped, so a restart
  cannot overheat the machine; it costs at most one extra log line.
- **No pid file, no self-daemonising, no log file.** systemd and journald
  already provide these.
- **No moving average.** The asymmetric curve covers the same need without
  delaying the rising edge.
