# fanctl

基于 Rust 的服务器风扇控制守护进程：读 IPMI / NVML 温度，按分区曲线算出 PWM，
再用 `ipmitool raw` 写回主板，替代 BIOS 里那种"要么吵、要么热"的粗糙逻辑。

目标环境：**Tyan S8030 + AMD EPYC 7413 + NVIDIA GPU**，Linux + systemd。
硬件差异全部落在 `platform.toml`，控制逻辑里没有一处十六进制常量。

当前状态：`cargo build` / `cargo test`（58 个测试）/ `cargo clippy --all-targets`（零告警）/
`cargo fmt --check` 全部通过；控制循环已用假的 `ipmitool` 做过端到端验证。
**尚未在真实 BMC 上跑过**，上机前务必先跑 `check` 与 `--dry-run`（见"安全须知"）。

---

## 1. 快速开始

```bash
cargo build --release

sudo install -Dm755 target/release/fanctl /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml   /etc/fanctl/config.toml

# 1) 静态校验 + 硬件探测（会真的执行 ipmitool sdr / sdr get / 一次批量 raw 读）
sudo fanctl check

# 2) 只读演练：读温度、算档位、打日志，不发任何控制指令
sudo fanctl run --dry-run -v

# 3) 正式运行（前台，交给 systemd 托管）
sudo fanctl run
```

systemd：

```bash
sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
journalctl -u fanctl -f
```

常用参数：`-c/--config`、`-p/--platform`（可与默认路径不同）、`-v`/`-vv`、`run --dry-run`。
`-v` 调整日志级别（默认 INFO，`-v` DEBUG，`-vv` TRACE）；`RUST_LOG=debug` 可覆盖它，
但只接受全局级别（`error|warn|info|debug|trace|off`），不支持 `fanctl::control=trace` 这种按模块的指令——
写了会打印一行提示并回落到 `-v`，不会静默失效。
退出码：`0` 正常（含收到 SIGTERM 后优雅退出），`1` 失败（配置错误、ipmitool 不可用等）。

---

## 2. 项目结构

```
fanctl/
├── Cargo.toml                 # 依赖、feature、lints（unsafe 禁止、clippy::all）
├── rust-toolchain.toml        # stable + rustfmt/clippy
├── src/
│   ├── main.rs                # 只做：解析 CLI → 初始化日志 → 调 app::run
│   ├── lib.rs                 # 库入口，分层文档
│   ├── error.rs               # 两个错误枚举：Error / CommandError
│   ├── logging.rs             # tracing → stdout（journald，无 ANSI），进程级配置一次
│   ├── actuator.rs            # 唯一写主板的地方（渲染模板 + ipmitool）
│   ├── app.rs                 # 子命令编排：check / run，信号注册
│   ├── app/
│   │   └── cli.rs             # clap 定义
│   ├── config.rs              # 加载 + 两份文件的交叉校验
│   ├── config/
│   │   ├── defaults.rs        # 两份配置的全部默认值（唯一一处）
│   │   ├── platform.rs        # platform.toml：引脚 id、传感器名、指令模板
│   │   └── policy.rs          # config.toml：全局、分区、联动
│   ├── sensors.rs             # SensorHub、Source、SensorSnapshot
│   ├── sensors/
│   │   ├── ipmi.rs            # 批量 raw 读 + sdr 解析（纯函数，可单测）
│   │   └── gpu.rs             # nvml-wrapper，惰性初始化 + feature 开关
│   ├── control.rs             # 聚合（max/mean）
│   ├── control/
│   │   ├── curve.rs           # 非对称防抖状态机
│   │   ├── merge.rs           # 冲突取大 → 联动 → 上下限
│   │   └── engine.rs          # 控制循环、失败策略、优雅退出
│   ├── util.rs                # 只收纳跨层共用的小工具
│   └── util/
│       ├── name.rs            # 名字归一化（大小写不敏感）
│       └── process.rs         # 子进程：超时即杀、非零退出即错误
├── examples/                  # 可直接用的 platform.toml / config.toml
├── packaging/fanctl.service   # systemd 单元模板
└── tests/                     # 集成测试（配置校验、控制逻辑、生命周期）
```

分层规则一句话：**`control` 只做决定，`actuator` 只做执行，`config` 只有数据。**
因此 `control/` 的全部逻辑都能在没有主板、没有 BMC 的机器上测试。

---

## 3. 配置

### 3.1 `platform.toml` — 硬件适配层

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `[platform].name` | 是 | — | 板名，仅用于日志 |
| `[platform].cmd_template` | 是 | — | `ipmitool` 参数模板，必须含 `{id}` 与 `{pwm}`；`{pwm}` 渲染为 `0xNN` |
| `[platform].ipmitool_path` | 否 | `ipmitool`（走 `$PATH`） | 建议在服务器上写绝对路径 |
| `[pins]` | 是，≥1 条 | — | 语义名 → 十六进制引脚 id，必须是 `0x00`–`0xFF` |
| `[sensors]` | 是，≥1 条 | — | 语义名 → `{ sdr = "…", id = "0xNN" }`；不允许占用 `GPU` / `GPU<n>` |

`[sensors]` 的每条要写两个值，两者都必须对：

```toml
[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
```

- `id` 是 IPMI **传感器编号**，控制循环用它读温度（`raw 0x04 0x2d <id>`）。
- `sdr` 是 `ipmitool sdr` 打印的名字，`check` 用它做诊断和交叉验证。

编号可以从 `ipmitool sdr get '<名字>'` 第一行拿到（`Sensor ID : CPU_Tctl_Value (0x1)`）。
两个语义名指向同一个编号会在加载期报错；`check` 还会把 raw 读数和 `ipmitool sdr`
比一遍，差超过 2 ℃ 就判失败——那意味着这块板子的温度不能直接当摄氏度用。

### 3.2 `config.toml` — 业务逻辑层

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `[global].interval_seconds` | 是 | — | 采样**周期**（≥1）。周期含本轮耗时，不是"跑完再睡这么久" |
| `[global].min_duty_cycle` | 是 | — | 全局下限，1..=100 且 ≤ `max_duty_cycle`（不允许停转） |
| `[global].max_duty_cycle` | 是 | — | 全局上限，≤100 |
| `[global].fallback_duty_cycle` | 是 | — | **所有**传感器都读不到时的保底转速，必须落在 `min_duty_cycle..=max_duty_cycle` |
| `[global].default_duty_cycle` | 是 | — | 待机转速；分区省略时继承它。必须落在 `min_duty_cycle..=max_duty_cycle`，且不得高于本分区第一档 |
| `[global].debounce_celsius` | 否 | `2.0` | 降档防抖（℃）；分区省略时继承它 |
| `[global].aggregation` | 否 | `max` | 聚合方式；分区省略时继承它 |
| `[global].command_timeout_seconds` | 否 | `10` | 单次 `ipmitool` 的超时，≥1 |

分区与联动：

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `[[zones]]` | 是，≥1 个 | — | 整段缺失在加载期报错 |
| `[[zones]].name` | 是 | — | 唯一（忽略大小写），仅用于日志 |
| `[[zones]].monitor_sources` | 是 | — | 非空；每项是 `[sensors]` 的键，或保留名 `GPU` / `GPU<n>` |
| `[[zones]].aggregation` | 否 | 继承 `[global]` | `max` 或 `mean`，只对**本周期读到值**的源生效 |
| `[[zones]].target_pins` | 是 | — | 非空，必须在 `[pins]` 里 |
| `[[zones]].default_duty_cycle` | 否 | 继承 `[global]` | 1..=100；低于第一档阈值时的转速，也是退出时写回的值 |
| `[[zones]].debounce_celsius` | 否 | 继承 `[global]` | 单侧防抖（只作用于降档），≥0 且必须小于相邻阈值间隔 |
| `[[zones]].thresholds` | 是，≥1 条 | — | 缺了在加载期报错 |
| `[[zones.thresholds]].temperature` | 是 | — | 必须严格升序 |
| `[[zones.thresholds]].duty_cycle` | 是 | — | 1..=100 |
| `[[follows]]` | 否 | 无规则 | 整段可以省略；写了才校验 |
| `[[follows]].pin` | 是 | — | 必须被某个 zone 驱动 |
| `[[follows]].min_of` | 是 | — | 同上；不能等于 `pin` |

上表"默认值"一列与 `src/config/defaults.rs` 是同一份信息，且由
`tests/config_validation.rs::an_omitted_timeout_gets_the_documented_default` 钉住：
只有 `ipmitool_path`、`command_timeout_seconds`、`debounce_celsius`、`aggregation`
四个字段**可省略且有初值**；其余要么必填，要么是"继承 `[global]`"而不是取默认值
（分区里的 `aggregation`、`default_duty_cycle`、`debounce_celsius` 属于后者）。
运行期调参（子进程轮询退避、关停轮询间隔、温度合理性区间）不在这个文件里，
`defaults.rs` 文件头有一张指路表指向它们的实际位置。

`aggregation`、`default_duty_cycle`、`debounce_celsius` 在 `[global]` 里是**共享策略**：
分区不写就继承，写了就覆盖。继承在加载时一次性展开（`FanConfig::from_raw`），
运行期每个分区拿到的都是确定值，不存在"空值再去看全局"的分支。

**名字一律大小写不敏感。** `cpu_fan`、`CPU_FAN`、`Cpu_Fan` 是同一个引脚，
`"gpu"` 与 `"GPU"` 等价，`"gpu1"` 与 `"GPU1"` 等价。加载时会把 `config.toml` 里的引用统一改写成
`platform.toml` 声明的大小写，之后再精确比较，所以 `check` 与 `run` 不会出现
"检查通过、运行找不到"的偏差。`platform.toml` 里只差大小写的两个键（`CPU_FAN`
与 `cpu_fan` 并存）会直接报错。SDR 名字还额外忽略分隔符：`CPU_Tctl_Value`、
`CPU Tctl Value`、`cpu-tctl-value` 视为同一个传感器。

校验项（`fanctl check` 与 `run` 启动时都执行，一次列出全部问题）：
TOML 语法与未知字段 → 阈值升序 → `debounce_celsius` 必须小于相邻阈值间隔 →
引脚/传感器必须存在 → 阈值与转速范围 → 联动规则无环、无自引用、两端都被某个分区驱动 →
`min_duty_cycle ≤ fallback_duty_cycle ≤ max_duty_cycle` →
`fallback_duty_cycle` 不低于任何分区的待机档（失去全部读数不该比"测到很凉"更安静）→
待机档在窗口内且不高于第一档 → 名字大小写冲突 → `[sensors]` 不得占用 `GPU` / `GPU<n>`。
`GPU<n>` 的编号是否存在要连上驱动才知道，属于运行期事实：`check` 会把实际枚举到的
`GPU0..n` 打出来，编号写错时那个分区会走"无读数"路径（保持上一档并告警），不会 panic。

> `deny_unknown_fields` 是刻意开的：写错字段名会直接报错，而不是安静地用了默认值。

---

## 4. 控制算法

### 4.1 采集与聚合

控制循环**不读整张 SDR 表**。`ipmitool sdr` 输出 53 行要 2.9 s，因为每一行都是一次
Get Sensor Reading 往返；按类型过滤（`sdr type temperature`）也一样慢。
循环只读 `[sensors]` 里被某个分区引用到的那些，一条一个 `raw 0x04 0x2d <id>`，
全部命令塞进一个 `ipmitool exec` 进程：

| 读法 | 1 个传感器 | 3 个 | 5 个 |
|---|---|---|---|
| `ipmitool sdr`（全表） | 2850 ms | 2850 ms | 2850 ms |
| 按名字 `sdr get` | 158 ms | 648 ms | ~1000 ms |
| **`exec` + raw（现在用的）** | **105 ms** | **122 ms** | **155 ms** |

（TYAN S8030 / ipmitool 1.8.19 实测。）

`ipmitool exec` 在某一条失败时不会停下，只是不为它输出——直接按行号对应就会把后面的
读数错位到别的传感器。所以每条读命令前面都插一条 `raw 0x06 0x01`（Get Device ID，
固定 15 字节）当分隔符：**空洞就是失败的那条**，其余照常使用。

raw 响应第一个字节是原始读数，第二个字节的 bit 5 表示"无读数"（坏传感器会回
`00 e0 c0 00`，不能当成 0 ℃）。这里不套用 SDR 记录里的 M/B 系数，所以 `check` 必须
验证这块板子确实是 1:1，否则整个方案不成立。

非温度（`RPM`、`no reading`、`ns`）记为"没有读数"而不是 0 —— 避免把缺数据的源当成低温。
GPUs 走 NVML，句柄惰性初始化：没装卡/驱动在重启时下个周期会自动重试。
`"GPU"` 只是 `monitor_sources` 里的一个关键字，与主板 SDR 无关：它把 NVML 下
所有卡的读数取最高，再作为一个源参与本分区的 `max`/`mean`。

需要只看某一张卡时写 `"GPU0"`、`"GPU1"`……编号就是 NVML 的 device index，
`check` 会把每个编号对应哪张卡、多少度打出来。典型场景是"两张 4080 共用一个冷排，
另一张 3080 自带风扇"：冷排分区写 `["GPU1", "GPU2"]`，那张自扇卡满载时就不会把
冷排一起拉起来。编号由驱动枚举顺序决定，**不换卡、不重装驱动就不会变**；
两张同型号的卡只能靠编号区分，不要按型号名猜。

`[sensors]` 里不允许再定义 `GPU` 或 `GPU<n>`（加载时直接报错），避免一个名字
指向两种数据源。

聚合只统计"本周期真有值"的源：`mean` 不会因为没插显卡被拖到 0 ℃。

采集是"每周期一次、一次解析"：`monitor_sources` 在加载期就解析成 `Source` 列表，
被引用的传感器名、SDR 名、编号也一次性整理好；循环里只遍历这个短列表，不碰字符串、
不重建切片。`platform.toml` 里声明了却没被任何分区引用的传感器**完全不读**。
没有任何分区引用 `GPU` 时，连 NVML 都不会打开（否则一个纯 CPU 的机器会每 10 秒
重试一次 `dlopen("libnvidia-ml.so")`）。

### 4.2 非对称防抖（快升慢降）
不引入滑动滤波，只用单侧迟滞，档位 `n` 对应第 `n` 个阈值：

| 方向 | 条件 | 动作 |
|------|------|------|
| 升档 | `温度 >= 阈值` | **立即**切档，可一次跳多档 |
| 降档 | `温度 <= 当前档阈值 - debounce_celsius` | 降一档 |
| 跌破第一档 | `温度 <= 第一档阈值 - debounce_celsius` | 直接回 `default_duty_cycle` |
| 死区内 | 以上都不满足 | 保持当前 PWM 不变 |

以 `55/70/85 → 40/60/100 %`、`debounce_celsius=2` 为例：70 ℃ 升到 60 %，要在
**68 ℃ 才降回 40 %**；但 71 ℃ 会立刻升回 60 %。升温不等，降温不抖。

**曲线的低端可能没有区分度。** 风扇有最低转速，占空比再往下压转速也不会更低。
实测某 TYAN S8030（TYAN 原厂扇）：30 % = 690 RPM 就是地板，40 % = 828，
60 % = 1242，100 % = 1932。所以 `min_duty_cycle` 和待机档若都落在地板以下，
写 25 还是 30 转速完全一样——**那些数字不生效，不是配置写错了**。想更安静只能改
BMC 自己的风扇策略（或换扇），fanctl 无法让一把扇子低于它自己的最低转速。
反过来说，按 30 % 待机是"物理上最安静"的选择，不必再往低调。

### 4.3 合并与兜底（顺序固定）
1. 各分区算出目标引脚 → **同一引脚取最大值**（绝不覆盖降速）；
2. `follows` 求不动点（与声明顺序无关）→ `Pwm(A) = max(Pwm(A), Pwm(B))`；
3. 夹到 `[min_duty_cycle, max_duty_cycle]`；
4. 只对"与上次成功下发值不同"的引脚发指令（BMC 不会被每 10 秒刷一遍）。

### 4.4 失败策略

| 情况 | 反应 |
|------|------|
| 某个分区读不到数据 | 保持该分区上一次的转速（不降速） |
| 该分区从没读到过数据 | 用 `fallback_duty_cycle` |
| 一个传感器都读不到 | 所有**被分区驱动**的引脚 → `fallback_duty_cycle`（没人管的引脚不碰，理由同退出复位） |
| `ipmitool` 写失败 / 超时 | 记 Error，**下个周期重试**，不退出、不更新"已下发值" |
| BMC 挂住不回 | 单次调用有超时；超时后**杀掉并回收子进程**（连同两个读管道的线程），循环不会被卡死，也不会每周期漏一个进程 |
| `ipmitool` 根本不存在 | 报 `IpmitoolMissing`，与"BMC 超时"区分——这一类不会自愈，`check` 直接失败 |
| SIGINT / SIGTERM | 每个被驱动的引脚 → 所属分区 `default_duty_cycle`，然后退出 0 |

---

## 5. 安全须知

1. **先 `check`，再 `--dry-run`，最后才正式跑。** `check` 会真跑一次整张 SDR 表、逐个
   `ipmitool sdr get`，再做一次批量 raw 读，确认 `[sensors]` 里的名字、编号、读数三者
   互相吻合；不一致就判失败，不会带着一个错的编号进入控制循环。
2. **引脚 id 只做了语法校验**。要确证 `0x00` 就是 CPU_FAN，只能实际写一次再听声音/看转速；
   `check` 只能按名称猜（猜不到会给 `[warn]`，不是错误）。第一次正式跑建议单独盯一次
   `journalctl -u fanctl -f`，确认"改的是哪把扇子"。
3. `min_duty_cycle ≥ 1` 是硬约束，配置里写 0 会直接报错。
4. 进程退出时 BMC 保留最后一次写入的值，所以 `SIGTERM` 会把每个被驱动的引脚写回
   所属分区的 `default_duty_cycle`；**没有任何分区驱动的引脚不会被碰**（daemon 从没写过它，
   退出时也不该凭空塞一个值，`check` 会给出 `[warn]`）。
   `TimeoutStopSec` 必须覆盖最坏情况 `(1 + 2N) × command_timeout_seconds`（N = 被驱动的引脚数：
   退出时可能还有一轮在跑的读 + N 次写，加上复位 N 次写），否则 systemd 会在优雅退出做到一半时 SIGKILL。
5. `ipmitool` 需要 root（或 udev 规则 + `CAP_SYS_RAWIO`）；unit 里刻意**没有**加
   `PrivateDevices=yes` 这类会屏蔽 `/dev/ipmi*` 的选项。
6. BIOS 里若把风扇策略设成 "Full Speed" 或某些 BMC 会在重启后重置 PWM，
   需要相应的 `ipmi` 初始化或接受"重启后由 daemon 下一个周期纠正"。

---

## 6. 已知限制 / 后续可做

- **没有 SIGHUP 热重载**：改配置需 `systemctl restart`（有意为之，配置错误时不会带着半套参数跑）。
- **`command_timeout_seconds` 默认 10 s 是按实测定的**：BMC 读全表常常要 2～3 s（TYAN S8030
  实测 2.9 s，满负载时抖到 6 s），给 10 才有余量。周期是"含本轮耗时的周期"，
  但 SDR 读取时间超过 `interval_seconds` 时循环会变成连续采样，日志里会出现
  "cycle overran the interval"。
- **状态不落盘**：重启后从当前温度重新定档（升温路径本来就无防抖，不会因此过热）。
- 引脚存在性只能启发式核对（见安全须知第 2 条）。
- 若要支持多块板/多套策略：`platform.toml` 已经解耦，加个 `--platform` 切换即可。

---

## 7. 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release            # 需要 GPU 支持
cargo build --release --no-default-features   # 无 NVIDIA 驱动的机器
```

- Edition 2021，`rust-version = "1.84"`。这个下限不是语言特性要求的，而是 `resolver = "3"`
  （MSRV 感知的依赖解析）的下限：更老的 cargo 连 manifest 都读不进去。部署时用 rustup 的 stable
  即可。`Cargo.lock` 入库，保证部署可复现。
- `#![forbid(unsafe_code)]`；`[lints.clippy] all = "warn"`。
- **release 用 `panic = "abort"`**：这个 crate 没有 `catch_unwind`，生产代码里也没有
  `unwrap` / `expect` / `panic!`，会让栈展开的唯一场合是"打了个不该有的 bug"——此时进程
  无论如何都要死（风扇恢复只走 SIGINT/SIGTERM，两条路都不经过 panic）。改 abort 后，
  每条 unwind 清理路径、`.eh_frame`、`.gcc_except_table` 都不再生成，实测体积 −29%。
  代价是 panic 时只打印 `panicked at src/…:line:col`，没有符号化调用栈；需要栈时临时重编：
  `cargo build --release --config 'profile.release.panic="unwind"'`。
- 所有需要硬件才能验证的东西都被推到边界：`ipmi::parse_sdr`、`curve`、`merge` 全是纯函数，
  测试不依赖 BMC、不依赖 GPU。
