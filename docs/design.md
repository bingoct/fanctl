# Design

**English** · [简体中文](design-cn.md)

How one control cycle works. Configuration fields are in
[config.md](config.md); measured timings are in [cost.md](cost.md).

## The cycle

One thread, one loop, no async runtime:

```text
   ┌──────────────────────────────────────────────────────────────┐
   │ 1  read      every sensor a zone references, in one ipmitool │
   │              exec: temperatures from the BMC, GPUs from NVML │
   ├──────────────────────────────────────────────────────────────┤
   │ 2  aggregate per zone: max or mean over the sources that     │
   │              reported this cycle                             │
   ├──────────────────────────────────────────────────────────────┤
   │ 3  curve     per zone, temperature -> duty cycle, with       │
   │              hysteresis on the way down only                 │
   ├──────────────────────────────────────────────────────────────┤
   │ 4  merge     one pin, several zones -> the highest demand;   │
   │              then [[follows]]; then clamp to [min, max]      │
   ├──────────────────────────────────────────────────────────────┤
   │ 5  write     only pins whose value changed, in one ipmitool  │
   │              exec, confirmed per pin by the BMC's reply      │
   └──────────────────────────────────────────────────────────────┘
                        sleep until the next period
```

Steps 2 to 5 are pure functions over owned data, so the control logic runs
without a BMC, a GPU or a board: `tests/control_logic.rs` plus unit tests next
to each module.

The period covers the work. A 10 s period with a 130 ms read sleeps 9.87 s. A
cycle that overruns the period is logged and the next one starts immediately
instead of shifting the schedule.

The configuration is read once. A change means a restart, so a half-valid file
cannot replace a running configuration.

## Reading

`ipmitool sdr` walks the whole Sensor Data Repository, one round trip per row:
2.9 s on the S8030. fanctl requests only the sensors its zones reference, as
`raw 0x04 0x2d <sensor-number>`, and sends all of them to a single `ipmitool
exec` through stdin.

`exec` does not stop at a failing command: it skips the output and continues. If
readings were matched by line, one failed sensor would shift every following
reading onto the wrong name. Each read is therefore preceded by a separator
command with a fixed output length (`raw 0x06 0x01`), and the reply is split
into slots:

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

A raw reply is not an SDR reading. Its first byte is the raw value, and whether
that value is a temperature or a tachometer count depends on the sensor. fanctl
does not apply the SDR record's M/B coefficients. That is only safe because
`check` verifies it against the same board: the raw value multiplied by the
coefficient has to agree with `ipmitool sdr` within 2 °C, otherwise `check`
fails. A reading reported as "no reading", `ns` or `disabled` is treated as
absent, never as 0 °C.

## Aggregation

A zone reduces its sources to one temperature per cycle.

- `max` (default): the zone protects the hottest source it can see, so one hot
  card cannot be averaged away by three cool ones.
- `mean`: for a zone whose sources share the same air, such as a radiator or a
  drive bank.

Only sources that reported this cycle are counted. `mean` over one live and one
dead sensor is the live sensor's temperature. A zone with no live source is
covered by the [failure policy](#when-a-reading-disappears).

`GPU` is a keyword rather than a sensor: the hottest of every NVIDIA card.
`GPU1` is one specific card by NVML index. Two zones can therefore watch
different subsets of the same hardware:

```toml
# The radiator cools these two cards only.
[[zones]]
name = "radiator"
monitor_sources = ["GPU1", "GPU2"]

# The front panel feeds the whole case, self-fanned cards included.
[[zones]]
name = "intake"
monitor_sources = ["CPU_Tctl", "GPU"]
```

A card with its own fans heats the case without being cooled by the radiator.
Watching `GPU` in the radiator zone would run the radiator for heat it does not
remove; watching `GPU1`/`GPU2` in the intake zone would under-feed the case.

## The curve

A zone's `thresholds` are steps: at or above `temperature`, use `duty_cycle`,
until the next step. Above the last step the last duty cycle holds; below the
first step the zone uses `default_duty_cycle`.

The curve is asymmetric:

| Direction | Condition | Action |
| --- | --- | --- |
| up | `temperature >= step.temperature` | switch immediately, possibly several steps |
| down | `temperature <= current step.temperature - debounce_celsius` | drop one step |
| below the first step | `temperature <= first.temperature - debounce_celsius` | back to `default_duty_cycle` |
| neither | — | hold |

With steps at 55/70/85 → 40/60/100 % and `debounce_celsius = 2`, 70 °C gives
60 %, and the fan returns to 40 % at 68 °C. A 71 °C reading snaps back to 60 %
without delay.

A moving average would also delay the rising edge, which is the direction that
matters under load. The hysteresis state is one integer, not a history buffer.

`debounce_celsius` has to be smaller than the gap between consecutive steps,
otherwise a step could never be left; the validator rejects such a file.

One physical limit affects curve design: below a fan's minimum speed, a lower
duty cycle changes nothing. On the S8030, 25, 28 and 30 % all produce 828 RPM
with the stock fans. A step written below that floor is not a configuration
error, it is a fan that cannot turn slower.

## Merging

Zones are evaluated independently, so their results can conflict. The
resolution order is fixed:

1. **One pin, several zones: the highest duty cycle.** Any other rule would let
   a zone that happens to be evaluated later reduce cooling another zone
   requested.
2. **`[[follows]]` to a fixed point.** `pin = A, min_of = B` means
   `A = max(A, B)`. Rules are applied repeatedly until nothing changes, so the
   result does not depend on the order they were written in. A cycle of rules, a
   self-reference, or an end that no zone drives is rejected at load time.
3. **Clamp to `[min_duty_cycle, max_duty_cycle]`**, last, so neither a zone nor
   a coupling rule can leave the global bounds.
4. **Write only what changed**, compared against the last confirmed value, so
   the BMC is not rewritten with the value it already holds.

`[[follows]]` covers constraints between pins that are not a curve, such as
"the intake is never slower than the radiator". Encoding that as a second curve
on the same pin would conflict with the first one.

## When a reading disappears

Three failures, three behaviours.

**A zone lost its reading this cycle.** Hold the previous duty cycle, so a
sensor that misses one cycle does not produce an audible step. After
`fallback_after_cycles` consecutive misses (default 2, 20 s at a 10 s period)
the zone drops to its own `fallback_duty_cycle` and logs it. One failed
`ipmitool exec` is a normal event on a busy BMC, hence the grace period.

**No sensor can be read at all.** Skip the grace period and write the fallback
values immediately. Holding a value chosen for a load nobody can see is worse
than a known-safe value.

**A write was not confirmed.** The pin keeps its previous value in the "last
sent" map and the next cycle writes again. An unconfirmed write is never
recorded as successful.

Two rules make the fallback value dependable:

- `fallback_duty_cycle >= default_duty_cycle`, enforced per zone, so a zone
  with no reading is never the quietest state of the machine.
- A zone may override both values. A coolant pump uses
  `default_duty_cycle = fallback_duty_cycle = 100`, which holds at idle, with no
  reading, and during shutdown.

### Shutdown

On `SIGINT` or `SIGTERM` every driven pin is written back to its zone's
`default_duty_cycle`, in one batch, and the process exits 0. The BMC keeps the
last value it was given, so without this the machine stays at whatever the last
load demanded.

Pins that no zone drives are not written, neither during the loop nor on exit.
`check` reports such a pin as `[warn]`, which is expected for a channel declared
only for its tachometer.

## Per-cycle log

Every cycle emits one line:

```text
cycle temps CPU_Tctl=23.0 GPU0=28.0 GPU1=26.0 | fan CPU_FAN=690 SYS_FAN_1=966 | duty cpu=30 intake=35
```

Three tables of `key=value` pairs, each introduced by a bare word. Names are
sorted, because the readings live in a hash map and an unstable order would make
two identical cycles look different.

The line is both machine-readable (it is the data source for a temperature
curve) and the routine proof that the loop is still running. A line every cycle
would be 8 600 lines a day for an idle machine, so it is emitted once every five
minutes and at DEBUG in between:

| Event | Level |
| --- | --- |
| 5 minutes since the last cycle line | INFO |
| under 5 minutes, nothing else to say | DEBUG |
| a duty cycle changed | INFO, with the temperatures attached |
| a driven fan went from turning to zero | WARN |

Nothing else decides the level. An earlier version also promoted the line when
any temperature moved 3 °C, to record transients between heartbeats. On an idle
machine that threshold sits exactly on the flicker of `CPU_Tctl`, which wanders
21-24 °C: 189 of 295 intervals in one day were temperature triggers, 113 of them
that flicker, and the real heartbeat got through only 60 times. A temperature
that moved is interesting when it moved a fan, and that case has a line of its
own:

```text
duty cycle changed pin=SYS_FAN_2 from=30 to=35 temps=CPU_Tctl:26.0,GPU0:55.0,GPU1:26.0
```

The cause travels with the effect, so a five-minute sample never has to explain
a step that happened four minutes earlier.

The first write of each pin after start is logged as `duty cycle initialised`
and carries no temperature: nothing caused it, the daemon is just asserting the
curve on a pin whose previous value it never saw.

Fan speed is in the line but not in the decision. A tachometer reading wanders
by a count or two on its own - the pump at 100 % alternates between 5934 and
6072 RPM, one count of 138 - and a speed that moved while the temperature did
not does not call for a line. A fan reaching zero does: the duty cycle says what
was requested, the speed is what the fan did, and a stalled rotor, a loose cable
or a header that lost its rail is visible only in the speed. That is logged once,
on the transition, rather than every cycle.

## Safety and limits

What the software checks:

- **`check` before `run`.** Validates both files, verifies every sensor number
  against the live BMC by name, cross-validates one raw read against
  `ipmitool sdr` (within 2 °C), lists the GPUs, and performs one batched read.
  It never writes a PWM value.
- **`--dry-run` before a real run.** Runs the whole loop and logs what it would
  have written.
- **A duty cycle of 0 is rejected.** `min_duty_cycle >= 1`; stopping a server
  fan is not a state this program can express.

What it cannot do:

- **Channel ids are only checked syntactically.** Only writing to a channel
  proves which header it is. `check` infers the association from names and warns
  when it cannot.
- **No watchdog.** If the process dies, the BMC keeps the last written values;
  restarting is systemd's job. A daemon stopped for a long time leaves the fans
  at the last value, or at standby if it shut down cleanly.
- **The BMC can take the fans back.** A reboot, a power cycle or a switch to
  "Smart Fan" in the BMC discards the raw writes. The daemon corrects them on
  the next cycle, so a machine that has just rebooted spends a few seconds under
  vendor control.

## Non-goals

- **No hot reload.** A restart is cheap and atomic; a half-applied configuration
  is neither.
- **No state on disk.** The step is derived from the current temperature at
  start. Rising temperatures are undamped, so a restart cannot overheat the
  machine; it costs at most one extra log line.
- **No pid file, no self-daemonising, no log file.** systemd and journald
  already provide these.
- **No averaging filter.** The asymmetric curve covers the same need without
  delaying the rising edge.
