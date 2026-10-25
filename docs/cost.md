# Cost

**English** · [简体中文](cost-cn.md)

What fanctl spends to control a fan, measured on a TYAN S8030GM2NE (AST2500,
ipmitool 1.8.19, Debian 13) on 2026-10-25. Every number comes from that machine.

Related: [design](design.md) · [configuration](config.md) · [platform](platform.md)

## A cycle, all in

| Step | Time | Notes |
| --- | --- | --- |
| Read every sensor a zone uses (4 temperatures + 3 tachometers) | 183 ms | one `ipmitool exec` |
| GPUs, first call in the process | 31 ms | `dlopen` of the NVIDIA library |
| GPUs, afterwards | < 1 ms | 5 µs per device |
| Aggregate, curve, merge | microseconds | arithmetic over a handful of values |
| Write changed pins (4) | 135 ms | one `ipmitool exec`, only when something changed |
| **Total, steady state** | **~185 ms per 10 s cycle** | 1.8 % of a core, 28 MB resident |

CPU time after 7 h 36 min of uptime: **5.5 seconds**, 0.02 % of one core. Idle,
the process sleeps between cycles and writes nothing.

## Reads

`ipmitool sdr` walks the whole Sensor Data Repository and issues a Get Sensor
Reading for every row: 53 rows, 53 round trips, **2.9 s** per call, independent
of how many values are wanted.

| Method | 1 reading | 4 | 7 |
| --- | --- | --- | --- |
| `ipmitool sdr` (whole table, 53 rows) | 2 925 ms | 2 925 ms | 2 925 ms |
| `ipmitool sdr get <name>`, one process per name | 156 ms | — | — |
| **`ipmitool exec` + raw** (what fanctl does) | **88 ms** | **130 ms** | **183 ms** |

Best of five runs each, control loop stopped so nothing else uses the BMC.

The unit of cost is the process, not the sensor. `ipmitool` needs ~75 ms to
start, read `/dev/ipmi0` and open the interface; each additional
`raw 0x04 0x2d` inside the same process costs 13–17 ms. Seven values in one
process are cheaper than one value in two processes.

`sdr get` does not scale either: it is one process and one repository walk per
name, 156 ms for one sensor and 2.6 s for three in the same measurement. A
sensor the BMC answers slowly delays every call in the same invocation. The
daemon therefore never uses it on the hot path; `check` uses it once, to verify
the numbers.

`exec` runs a command list from stdin:

```text
raw 0x06 0x01                                   # separator, 15 bytes
raw 0x04 0x2d 0x01                              # CPU_Tctl
raw 0x06 0x01
raw 0x04 0x2d 0x60                              # CPU_FAN tachometer
```

A separator before each command makes a failing command leave a hole instead of
shifting the rest onto the wrong names, see [design](design.md#reading). The
separators cost nothing measurable: seven readings with separators (183 ms)
versus four without (130 ms) is the same per-reading slope.

## Writes

Same mechanism:

| Method | 4 pins |
| --- | --- |
| `ipmitool raw` per pin, four processes | 419 ms |
| **one `ipmitool exec`** | **135 ms** |

Single runs each; earlier measurements on the same machine gave 356 ms and
125 ms. BMC latency varies by tens of milliseconds, so these are "roughly 3×
faster" rather than exact figures.

Only pins whose confirmed value changed are written, so a machine at a steady
temperature writes nothing: ten minutes of live control produced four write
commands in the first cycle and none afterwards.

Success is decided per pin by the BMC's acknowledgement (`fd 19 00`, three
bytes), not by `ipmitool`'s exit code. `exec` continues after a failed command
but still exits 1, so the exit code cannot say which pin failed; the slot
protocol answers that per pin.

## Binary size

1 505 464 bytes (1.44 MiB), stripped, `lto = "thin"`, `codegen-units = 1`,
`panic = "abort"`. Linked against libc and libgcc_s only.

| Section | Bytes | Share of file |
| --- | --- | --- |
| `.text` (code) | 1 200 774 | **79.8 %** |
| `.rodata` (constants, strings, tables) | 90 630 | 6.0 % |
| `.eh_frame` + `.eh_frame_hdr` (unwind tables) | 99 352 | 6.6 % |
| `.rela.dyn` (dynamic relocations) | 57 456 | 3.8 % |
| `.gcc_except_table` | 5 860 | 0.4 % |
| headers, symbol-free metadata, alignment | ~51 400 | ~3.4 % |

### Where the code comes from

Attributing every `.text` symbol to the crate that defined it (demangled names,
1 194 564 bytes accounted for):

| Crate | Share of code | What it is |
| --- | --- | --- |
| `fanctl` | **17.9 %** | this program, ~5 200 lines |
| `clap_builder` | 17.9 % | command-line parsing |
| `core` | 14.2 % | `Option`, iterators, formatting |
| `std` | 9.5 % | the standard library |
| `toml_edit` | 8.2 % | TOML parsing (via `toml`) |
| `alloc` | 6.5 % | allocation |
| `gimli`, `addr2line`, `rustc_demangle` | 5.7 % | panic backtrace symbolisation |
| `nvml_wrapper_sys` | 4.6 % | NVIDIA driver bindings |
| unattributed | 2.8 % | glue with no crate in the symbol name |
| `winnow` | 2.1 % | parser combinators, under `toml` |
| `tracing_subscriber` | 1.9 % | the logging pipeline |
| `hashbrown` | 1.5 % | `HashMap` |
| ~45 smaller crates | 7.2 % | `signal-hook`, `miniz_oxide`, `strsim`, … |

Command-line parsing is as large as the daemon itself: `clap` accounts for 18 %
of the code for two subcommands and four flags. Colour support is already
disabled (`default-features = false`), which saves 81 KB; the remainder is the
derive machinery and the help renderer. A hand-written `match` on `argv` would
remove most of it and cost the generated `--help`.

`panic = "abort"` reduces the file by 29 % on this codebase, because it removes
every landing pad and most of `.eh_frame`. Unwind tables remain 6.6 % of the file
because `std` needs them for `catch_unwind`. The cost is a panic without a
symbolised backtrace; rebuild with `--config 'profile.release.panic="unwind"'`
when one is needed.

TOML plus its parser combinators account for ~10 %. The configuration is read
once per process, so this is bought for convenience.

The optional feature is measurable: `--no-default-features` (no NVML, no GPU
temperatures) builds to **1 399 248 bytes**, 106 KB smaller.

## Runtime footprint

| Build | RSS | Anonymous | Threads |
| --- | --- | --- | --- |
| default (NVML) | 28 MB | 21 MB | 2 |
| `--no-default-features` | 8 MB | 1.2 MB | 1 |

The difference comes from linking the NVIDIA driver, which maps its own heap
into the process. fanctl's own allocations are the 1.2 MB, a few kilobytes of
configuration plus the sensor snapshot. RSS was unchanged over a 30-second probe
after 7 hours of uptime.

## Compared with a Go daemon on the same board

[coolercontrol][cc] and similar projects address a different problem, since they
talk to hwmon. The closest relative is [`sonmihpc/AutoFan`][af], written for
this board family: Go, GPU temperatures, `ipmitool raw` writes.

| | AutoFan (Go) | fanctl |
| --- | --- | --- |
| Temperature source | NVML only, in process | IPMI raw batch + NVML |
| Sensors read over IPMI | none | 4–7, batched |
| CPU / case temperature | not read | yes |
| Processes per sample | 0 | 1 (`exec`) |
| Processes per duty change | one per pin, concurrent goroutines | 1 (`exec`) |
| NVML init | `nvml.Init()` + `Shutdown()` on every sample | once, lazily |
| Hysteresis / debounce | none | asymmetric, per zone |
| Zones / one pin, several sources | no | yes |
| Behaviour when a reading fails | device dropped from the list; if that empties it, temperature 0 → the lowest bucket | hold, then per-zone fallback after a grace period |
| Restore on SIGTERM | no | every driven pin → standby |
| Config | YAML, `mode: max\|mean`, threshold table | TOML, zones, coupling, per-zone limits |

AutoFan does not use `ipmitool sdr`; it calls NVML in process and spends no
processes on reading, which is cheaper per sample than fanctl's 183 ms and the
appropriate design for a program that watches only GPUs. It also cannot read the
CPU or the case air, has one curve per GPU set rather than per zone, and
initialises the driver library every interval. fanctl spends one `ipmitool`
process per cycle because it needs the BMC for CPU, VRM and air sensors, which
NVML does not expose; once that process exists, adding a sensor costs 13–17 ms.

On writes fanctl is roughly 3× faster per change, because it starts one
`ipmitool` per cycle instead of one per pin.

[af]: https://github.com/sonmihpc/AutoFan
[cc]: https://gitlab.com/coolercontrol/coolercontrol

## Reducing cost

1. **Raise `interval_seconds`.** 10 s is chosen for responsiveness, not cost;
   30 s is sufficient for a machine with litres of coolant.
2. **Reference fewer sensors.** Only sensors a zone names are read; declaring
   twenty in `platform.toml` costs nothing, watching them costs 13–17 ms each per
   cycle.
3. **Build without the GPU feature** on a host without an NVIDIA card: 106 KB of
   binary and 20 MB of resident memory.
4. **Drop the tachometers** if the log does not need speeds: seven readings
   become four, 183 ms becomes 130 ms.

Parsing is 560 ns per cycle and the process start dominates the remainder, so
nothing else is worth optimising.
