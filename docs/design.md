# Design

**English** · [简体中文](design-cn.md)

Why the daemon behaves the way it does. The configuration knobs it refers to are
in [config.md](config.md); the measured cost of each step is in
[cost.md](cost.md).

## The cycle

One thread, one loop, no async runtime:

```text
   ┌──────────────────────────────────────────────────────────────┐
   │ 1  read      every sensor a zone references, in one ipmitool │
   │              exec: temperatures from the BMC, GPUs from NVML │
   ├──────────────────────────────────────────────────────────────┤
   │ 2  aggregate per zone: max or mean over the sources that     │
   │              actually reported this cycle                    │
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

Steps 2 to 5 are pure functions over owned data. That is deliberate: the entire
control logic can be tested without a BMC, a GPU or a motherboard, and it is
(`tests/control_logic.rs`, plus unit tests next to each module).

The loop period includes the work. A 10 s period with a 130 ms read sleeps
9.87 s; it does not add the read on top. If a cycle overruns the period it says
so in the log and starts the next one immediately rather than drifting.

The daemon does not re-read the configuration, and does not daemonise itself.
`systemd` runs it in the foreground and restarts it; a configuration change is a
restart, so a half-valid file can never take over a running machine.

## Reading

The BMC is the bottleneck, and the obvious call is the wrong one: `ipmitool sdr`
walks the whole Sensor Data Repository, one round trip per row — 2.9 s on the
board this was written for. fanctl instead asks for exactly the sensors its
zones reference, as `raw 0x04 0x2d <sensor-number>`, and gets them all in a
single `ipmitool exec` process by feeding a command list on stdin.

`exec` does not stop at a failing command: it skips the output and carries on.
If the readings were matched by line, one bad sensor would shift every reading
after it onto the wrong name — a fan curve that looks plausible and controls the
wrong thing. So every read is preceded by a separator command with a fixed,
known output length (`raw 0x06 0x01`), and the reply is split into *slots*:

```text
raw 0x06 0x01            <- 15 bytes: slot starts
raw 0x04 0x2d 0x01       <- reading for CPU_Tctl
raw 0x06 0x01            <- next slot
raw 0x04 0x2d 0x20       <- no output: this sensor failed
raw 0x06 0x01
raw 0x04 0x2d 0x31       <- reading, still on the right sensor
```

A hole means "that one failed", not "shift everything left". The same protocol
serves the write path, where a hole means "that pin's write was not acknowledged"
(see [cost](cost.md#writes)).

Raw replies are not SDR readings: the first byte is the raw value, and whether
that value is a temperature or a tachometer count depends on the sensor. fanctl
does not apply the SDR record's M/B coefficients, which is only safe because
`check` verifies the assumption on the actual board (raw value × coefficient
must agree with `ipmitool sdr` within 2 °C, else it refuses to pass). A reading
marked "no reading", `ns`, or `disabled` is treated as *absent*, never as 0 °C —
a missing sensor must not look like a cold machine.

## Aggregation

Each zone reduces its sources to one temperature per cycle.

- `max` (the default) is the safe choice: a zone protects the hottest thing it
  can see, so a single hot card cannot be averaged away by three cool ones.
- `mean` is for a zone that genuinely serves a bulk — a bank of drives, a
  radiator whose fans all see mixed air.

Only sources that reported this cycle are counted. `mean` over one live sensor
and one dead one is that one sensor's temperature, not half of it. A zone with
no live source at all is the case the [failure policy](#when-a-reading-disappears)
covers.

`GPU` is not a sensor but a keyword: the hottest of every NVIDIA card. `GPU1`
is one specific card. Two zones, two views of the same hardware:

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

The distinction matters because a card with its own fans heats the case without
being cooled by the radiator. Binding the radiator to `GPU` would let that card
run the radiator for nothing; binding the intake to `GPU1`/`GPU2` would starve
it of cold air. Neither is expressible with a single "GPU temperature" setting,
which is why zones exist.

## The curve

A zone's `thresholds` are steps: at or above `temperature`, use `duty_cycle`,
until the next step. Above the last step the last duty holds; below the first,
the zone sits at `default_duty_cycle`.

The curve is asymmetric. Rising and falling are different events:

| Direction | Condition | Action |
|---|---|---|
| up | `temperature >= step.temperature` | switch **immediately**, possibly several steps |
| down | `temperature <= current step.temperature - debounce_celsius` | drop one step |
| below the first step | `temperature <= first.temperature - debounce_celsius` | back to `default_duty_cycle` |
| neither | — | hold |

Heat is a safety event and gets an instant reaction; cooling is a comfort event
and has to prove itself. With steps at 55/70/85 → 40/60/100 % and
`debounce_celsius = 2`, 70 °C brings 60 %, but the fan only returns to 40 % at
68 °C. A 71 °C blip snaps back to 60 % without waiting.

This buys more than a moving average would: a filter delays the *rise* too,
which is exactly the wrong direction when a GPU has just started a job. It also
costs nothing to reason about — the state is one integer, not a history buffer.

`debounce_celsius` must be smaller than the gap between consecutive steps;
otherwise a step could never be left, which the validator rejects.

One physical limit is worth knowing before tuning a curve: **below a fan's
minimum speed, a lower duty cycle changes nothing.** On the S8030, 25 %, 28 %
and 30 % all produce 828 RPM on the stock fans. A step written below that floor
is not a configuration error, it is a fan that cannot go slower — and the only
fix is a different fan or the BMC's own profile, not a bigger number in the
file.

## Merging

Zones are written independently, so they collide. The resolution order is fixed:

1. **One pin, several zones → the highest demand.** Taking the maximum is the
   only safe rule: any other choice would let a zone that happens to run later
   in the file reduce cooling another zone has already asked for.
2. **`[[follows]]` to a fixed point.** `pin = A, min_of = B` means
   `A = max(A, B)`. Rules are applied repeatedly until nothing changes, so the
   result does not depend on the order they were written in. A cycle of rules
   (`A` follows `B`, `B` follows `A`), a self-reference, or an end that no zone
   drives is rejected at load time.
3. **Clamp to `[min_duty_cycle, max_duty_cycle]`** — last, so neither a zone
   nor a coupling rule can escape the global bounds.
4. **Write only what changed**, compared against the last *confirmed* value, so
   the BMC is not rewritten every ten seconds with the number it already has.

Coupling exists because some relationships are not "a zone" — they are a
constraint between zones. "The intake is never slower than the radiator" is not
a curve, and encoding it as a second curve on the same pin would fight the
first one.

## When a reading disappears

Three different failures, three different answers.

**A zone lost its reading this cycle.** Hold the previous duty. A sensor that
blinks for one cycle must not produce an audible step. After
`fallback_after_cycles` consecutive misses (default 2, so 20 s at a 10 s period)
the zone gives up and drops to its own `fallback_duty_cycle`, and says so in the
log. The grace period exists because a single failed `ipmitool exec` is a normal
event on a busy BMC.

**Nothing can be read at all.** Skip the grace period and go straight to the
fallback values. This is not a blip; the BMC or the tool is gone, and holding a
value chosen for a load nobody can see is worse than a known-safe one.

**A write was not confirmed.** The pin keeps its old value in the "last sent"
map, so the next cycle tries again. A failed write is never remembered as
successful — that is the failure mode that would silently leave fans at standby
while the log claims success.

Two rules make the fallback trustworthy:

- `fallback_duty_cycle ≥ default_duty_cycle`, enforced **per zone**. Being blind
  must never be the quietest state the machine can reach.
- A zone may override both values, which is how a coolant pump is expressed:
  `default_duty_cycle = fallback_duty_cycle = 100`. Blind, idle, shutting down,
  sensor dead — it does not matter, the pump runs.

### Shutdown

On `SIGINT` or `SIGTERM` every *driven* pin is written back to its zone's
`default_duty_cycle`, in one batch, and the process exits 0. The BMC keeps the
last value it was given, so quitting without this would leave the machine at
whatever the last load demanded — usually the loudest it has ever been.

Pins that no zone drives are not touched, on the way in or on the way out. A
pin nobody wrote has no value fanctl is entitled to overwrite; if you keep a
channel declared only for its tachometer, `check` says so once and that is the
whole story.

## What the log line is for

Every cycle emits one line:

```text
cycle temps CPU_Tctl=23.0 GPU0=28.0 GPU1=26.0 | fan CPU_FAN=690 SYS_FAN_1=966 | duty cpu=30 intake=35
```

Three tables, `key=value` throughout, each introduced by a bare word so it can
be split without guessing. Names are sorted, because the readings live in a hash
map and an unstable order would make two identical cycles look different.

The line is deliberately both machine-readable and human-readable: it is the
data source for a temperature curve, and it is the only routine proof that the
loop is still turning. Those two jobs conflict — a line every cycle is 8 600
lines a day for an idle machine — so it is rate limited by *change*:

| Event | Level |
|---|---|
| any temperature moved ≥ 3 °C | INFO |
| any fan speed moved more than one tachometer count | INFO |
| a sensor or tachometer appeared or disappeared | INFO |
| nothing moved, under 5 minutes since the last INFO line | DEBUG |
| nothing moved, 5 minutes elapsed | INFO |
| a driven fan went from turning to zero | WARN |

The two thresholds are derived rather than chosen. The BMC reports whole
degrees, and an idle EPYC wanders between 22 and 24 °C all day, so a 2 °C step
still logged three lines a minute with nothing running; 3 °C is above the
flicker, and a real load crosses it within a cycle or two. Speed is quantised by
the tachometer itself — the pump at 100 % alternates between 5934 and 6072
forever, one count of 138 RPM — so one count is noise and two counts are motion.

Fan speed belongs in that comparison even though no duty cycle is computed from
it, because the two halves of the pair are not equivalent: the duty cycle is
what was *asked* of a header, the speed is what the fan *did*. A dead fan, a
loose cable or a header that lost its rail is invisible in the duty cycle and
obvious in the speed. That is also why a fan going to zero gets a WARN of its
own, once, on the transition instead of every cycle — a warning that repeats is
a warning that gets ignored.

## Safety and limits

What the software checks, and what it cannot:

- **`check` before `run`.** It validates both files, verifies every sensor
  number against the live BMC by name, cross-validates one raw read against
  `ipmitool sdr` (within 2 °C), lists the GPUs, and performs one batched read.
  It never writes a PWM value.
- **`--dry-run` before a real run.** Runs the whole control loop, logs what it
  would have written, sends nothing.
- **Channel ids are only checked syntactically.** Only writing to a channel
  proves which header it is; `check` guesses the association from names and
  warns when it cannot. The first real run deserves an operator watching
  `journalctl -u fanctl -f`, listening.
- **A duty cycle of 0 is rejected.** `min_duty_cycle ≥ 1` — stopping a server
  fan is not a state this program can express.
- **No watchdog inside the daemon.** If the process dies, the BMC keeps the last
  written values; that is what systemd's restart is for. If the daemon is
  stopped for long, the fans stay at the last value (standby, if it shut down
  cleanly) — not at full speed.
- **The BMC can take the fans back.** A reboot, a power cycle or a switch back
  to "Smart Fan" in the BMC discards the raw writes. The daemon corrects it on
  the next cycle, but a machine that has just rebooted spends a few seconds
  under vendor fan control.

## Deliberate non-goals

- **No hot reload.** A restart is cheap and atomic; a half-applied configuration
  is not.
- **No state on disk.** Bands are re-derived from the current temperature on
  start. Rising is undamped, so a restart cannot cause overheating, only one
  extra step in the log.
- **No `pid` file, no self-daemonising, no log file.** systemd and journald
  already do these jobs, and doing them twice is how daemons get out of sync
  with their unit files.
- **No averaging filter.** The asymmetric curve covers the same need without
  delaying the rising edge.
