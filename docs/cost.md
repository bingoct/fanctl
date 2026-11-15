# Cost

**English** · [简体中文](cost-cn.md)

What it costs to control one fan. Every number was measured on a TYAN
S8030GM2NE (AST2500, ipmitool 1.8.19, Debian 13, 2026-10-25).

Related: [design](design.md) · [configuration](config.md)
· [platform](platform.md)

## Total cost of one cycle

| Step | Time | Notes |
| --- | --- | --- |
| Read every sensor a zone uses (4 temperatures + 3 tachometers) | 183 ms | one `ipmitool exec` |
| GPUs, first call in the process | 6 ms | `dlopen` + `nvmlInit`; 31 ms with uncached library pages |
| GPUs, every call after that | 8 µs | three cards, handles cached |
| Aggregate, look up the curve, merge | microseconds | arithmetic over a handful of values |
| Write the pins that changed (4) | 135 ms | one `ipmitool exec`, only when values changed |
| **Steady-state total** | **~185 ms per 10 s cycle** | 1.8 % of one core, PSS 24–27 MB with GPUs |

After 7 h 36 min of continuous running, cumulative CPU time is **5.5
seconds**, 0.02 % of one core. Idle, the process sleeps between cycles and
produces no writes.

## Reads

`ipmitool sdr` walks the entire sensor table and issues a Get Sensor Reading
per row: 53 rows, 53 round trips, **2.9 s** per call, regardless of how many
values are wanted.

| Method | 1 reading | 4 | 7 |
| --- | --- | --- | --- |
| `ipmitool sdr` (whole table, 53 rows) | 2 925 ms | 2 925 ms | 2 925 ms |
| `ipmitool sdr get <name>`, one process per name | 156 ms | — | — |
| **`ipmitool exec` + raw** (what fanctl does) | **88 ms** | **130 ms** | **183 ms** |

Best of five runs each; the control loop was stopped during measurement and
no other process accessed the BMC.

The unit of cost is the process, not the sensor. `ipmitool` takes ~75 ms to
start and open `/dev/ipmi0` and the interface; each additional
`raw 0x04 0x2d` inside the same process adds 13–17 ms. Reading seven values
in one process is cheaper than reading one value in two processes.

`sdr get` does not scale either: every name costs one process launch plus one
full table walk — 156 ms for a single sensor, 2.6 s for three in the same
measurement. A sensor the BMC answers slowly delays every reading in the
same call. The hot path therefore never uses it; `check` uses it once, to
verify the numbers.

`exec` reads a command list from stdin:

```text
raw 0x06 0x01                                   # separator, 15 bytes
raw 0x04 0x2d 0x01                              # CPU_Tctl
raw 0x06 0x01
raw 0x04 0x2d 0x60                              # CPU_FAN tachometer
```

A separator before each command makes a failed command leave an empty slot
instead of shifting later readings onto the wrong names, see
[design](design.md#reading). The separator's overhead is unmeasurable: the
per-command increment is the same for seven readings with separators
(183 ms) as for four without (130 ms).

## Writes

Same mechanism:

| Method | 4 pins |
| --- | --- |
| `ipmitool raw` per pin, four processes | 419 ms |
| **one `ipmitool exec`** | **135 ms** |

Single measurements each; earlier measurements on the same machine gave
356 ms and 125 ms. BMC latency varies by tens of milliseconds, so read this
as "roughly 3×", not as exact figures.

Only pins whose confirmed value changed are written, so a machine at a
steady temperature produces no writes: ten minutes of live control wrote
four commands in the first round and nothing after.

Each command's success is decided individually by the BMC's acknowledgement
(`fd 19 00`, three bytes), not by `ipmitool`'s exit code. `exec` continues
after a failed command but returns 1 overall, and the exit code cannot say
which pin failed; the slot protocol answers that per pin.

## Binary size

1 505 064 bytes (1.44 MiB), stripped, `lto = "thin"`, `codegen-units = 1`,
`panic = "abort"`. Statically linked except for libc and libgcc_s.

| Section | Bytes | Share of file |
| --- | --- | --- |
| `.text` (code) | 1 200 390 | **79.8 %** |
| `.rodata` (constants, strings, tables) | 90 614 | 6.0 % |
| `.eh_frame` + `.eh_frame_hdr` (unwind tables) | 99 352 | 6.6 % |
| `.rela.dyn` (dynamic relocations) | 57 456 | 3.8 % |
| `.gcc_except_table` | 5 860 | 0.4 % |
| file headers, section headers, alignment | 51 392 | 3.4 % |

### Where the code comes from

Every `.text` symbol attributed to the crate that defines it (after
demangling, 1 194 167 bytes accounted for):

| Crate | Share of code | What it is |
| --- | --- | --- |
| `fanctl` | **17.9 %** | this program, ~5 100 lines |
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

Command-line parsing is as large as the daemon itself: two subcommands, four
parameters, and `clap` takes 18 % of the code. Colour output is already
disabled (`default-features = false`), saving 81 KB; the rest is the derive
machinery and the help renderer. A hand-written `match` on `argv` would
remove most of it, at the cost of the generated `--help`.

`panic = "abort"` shrinks the file by 29 %: it removes every landing pad and
most of `.eh_frame`. Unwind tables still take 6.6 % because `std`'s
`catch_unwind` needs them. The cost is a panic without a symbolised
backtrace; rebuild with `--config 'profile.release.panic="unwind"'` when
one is needed.

TOML and its parser combinators account for ~10 %. The configuration is read
once per process, so this cost buys convenience.

The optional feature is measurable: `--no-default-features` (no NVML, no GPU
temperatures) builds to **1 399 024 bytes**, 106 KB smaller.

## Runtime footprint

RSS counts every page a process has mapped, including pages other processes
mapped first. The two numbers below separate the two kinds of page.

**What fanctl allocates itself** — heap, anonymous pages, stack. Not shared,
not reclaimable, and unaffected by other processes on the host.

**PSS** — shared pages divided by the number of processes mapping them, so
libc counts towards fanctl only in proportion. This number moves with the
host: measured twice on the same idle machine, it came out at 1.1 MB and
1.7 MB.

| `config.toml` | fanctl allocates | RSS | PSS |
| --- | --- | --- | --- |
| no GPU source | **336 KB** | 3.4 MB | 1.1–1.7 MB |
| GPU source, resident handle | **21.0 MB** | 28.8 MB | 24–27 MB |

The 336 KB without a GPU source is 112 KB of heap, 96 KB of anonymous pages
and 36 KB of stack, and it does not grow — RSS equals VmHWM on a service
that has run for hours. The remaining 3.1 MB is mappings: 1.3 MB of binary
code, 1.5 MB of libc, 220 KB of the loader, 104 KB of libgcc_s — all
read-only, dropped under memory pressure and read back on the next page
fault.

With a GPU source there is an extra 19 MB of anonymous driver heap (16 MB of
it one transparent huge page), plus 4.6 MB of `libcuda.so` and 1.2 MB of
`libnvidia-ml.so`. A 60-line C program that only links the library locates
the source:

| Stage | RSS |
| --- | --- |
| start | 1 476 KB |
| after `dlopen` | 2 260 KB |
| after `nvmlInit` | 26 656 KB |
| after reading three GPUs | 26 916 KB |
| after `nvmlShutdown` | 26 904 KB |
| after `dlclose` | 6 920 KB |

`nvmlShutdown` releases the driver's internal state but keeps the mapping,
so resident memory is unchanged; `dlclose` unloads the library and the 20 MB
driver heap is unmapped with it. The choice is therefore "resident handle"
versus "reopen every cycle", not "resident or not":

| | resident handle | reopen every cycle |
| --- | --- | --- |
| PSS | 24–27 MB | 3.5 MB |
| GPU read per cycle | 8 µs | 26.5 ms |
| `nvmlInit` | 5.5 ms, once | 5.5 ms, every cycle |
| `dlopen`, `dlclose`, driver setup | 0 | 21 ms, every cycle |

fanctl chooses the resident handle: reading three cards costs 8 µs against a
185 ms cycle, while reopening costs 26.5 ms per cycle — 14 % of the cycle —
to hand the 20 MB driver heap back to the kernel and remap it ten seconds
later. On a machine with no NVIDIA card, use `--no-default-features`: NVML
is not linked and the allocation stays at 336 KB.

Virtual address space is a larger number again: 404 MB, of which 296 MB is
anonymous address space the driver reserves for GPU address windows and
110 MB is the `libcuda.so` mapping. Reserved address space is not memory:
only 28.8 MB is resident, and the rest is never touched.

`scripts/rss-breakdown.sh` and `scripts/nvml-steps.c` reproduce these two
tables; `scripts/nvml-residency.c` is the measurement program behind the
"reopen every cycle" column. An earlier version of this document reported
"8 MB without NVML", which was wrong: it measured the RSS of `sudo`, not of
the process it started.

## Compared with a Go daemon on the same board

Projects like [coolercontrol][cc] address a different problem: direct hwmon
reads and writes. The closest implementation is [`sonmihpc/AutoFan`][af],
written for this board family: Go, GPU temperatures, `ipmitool raw` writes.

| | AutoFan (Go) | fanctl |
| --- | --- | --- |
| Temperature source | NVML only, in process | IPMI raw batch + NVML |
| Sensors read over IPMI | none | 4–7, batched |
| CPU / case temperature | not read | read |
| Processes per sample | 0 | 1 (`exec`) |
| Processes per duty change | one per pin, concurrent goroutines | 1 (`exec`) |
| NVML init | `nvml.Init()` + `Shutdown()` every sample | once, lazily |
| Hysteresis / debounce | none | asymmetric, per zone |
| Zones / one pin, several inputs | no | yes |
| Behaviour when a reading fails | device dropped from the list; if that empties it, temperature 0 → the lowest step | hold first, then per-zone fallback after a grace period |
| Restore on SIGTERM | no | every driven pin → standby value |
| Config | YAML, `mode: max\|mean`, threshold table | TOML, zones, coupling, per-zone limits |

AutoFan calls NVML in process, so reads consume no process and each sample
is cheaper than fanctl's 183 ms — an appropriate design for a program that
watches only GPUs. It also cannot read CPU or case-air temperatures, its
curves are organised per GPU set rather than per zone, and it reinitialises
the driver library every cycle — 26.5 ms, in exchange for not holding the
20 MB driver heap between samples. fanctl starts one `ipmitool` process per
cycle because it needs the BMC for the CPU, power and intake temperatures
that NVML does not expose; once the process exists, each extra sensor costs
13–17 ms.

On writes fanctl is roughly 3× faster per change: one `ipmitool` per cycle,
not one per pin.

[af]: https://github.com/sonmihpc/AutoFan
[cc]: https://gitlab.com/coolercontrol/coolercontrol

## Reducing cost

1. **Raise `interval_seconds`.** 10 s was chosen for responsiveness, not
   cost; 30 s is enough for a machine holding litres of coolant.
2. **Reference fewer sensors.** Only sensors a zone names are read;
   declaring twenty in `platform.toml` costs nothing, while reading them all
   each cycle costs 13–17 ms each.
3. **Drop the GPU feature on machines without an NVIDIA card**: 106 KB
   smaller binary, 20 MB less driver heap.
4. **Drop the tachometers when the log does not need speeds**: seven
   readings become four, 183 ms becomes 130 ms.

Parsing takes 560 ns per cycle; process startup dominates the rest.
