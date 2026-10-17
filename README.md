# fanctl

基于 Rust 的服务器风扇控制守护进程：读 IPMI / NVML 温度，按分区曲线算出 PWM，
再用 `ipmitool raw` 写回主板，替代 BIOS 里那种"要么吵、要么热"的粗糙逻辑。

目标环境：**Tyan S8030 + AMD EPYC 7413 + NVIDIA GPU**，Linux + systemd。
硬件差异全部落在 `platform.toml`，控制逻辑里没有一处十六进制常量。

当前状态：`cargo build` / `cargo test`（34 个测试）/ `cargo clippy --all-targets`（零告警）/
`cargo fmt --check` 全部通过；控制循环已用假的 `ipmitool` 做过端到端验证。
**尚未在真实 BMC 上跑过**，上机前务必先跑 `check` 与 `--dry-run`（见"安全须知"）。

---

## 1. 快速开始

```bash
cargo build --release

sudo install -Dm755 target/release/fanctl /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml   /etc/fanctl/config.toml

# 1) 静态校验 + 硬件探测（会真的执行一次 ipmitool sdr）
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
│   ├── app.rs                 # 子命令编排：check / run，信号注册
│   ├── cli.rs                 # clap 定义
│   ├── error.rs               # thiserror 单错误类型
│   ├── logging.rs             # tracing → stdout（journald）
│   ├── process.rs             # 带超时的子进程执行
│   ├── actuator.rs            # 唯一写主板的地方（渲染模板 + ipmitool）
│   ├── config/
│   │   ├── mod.rs             # 加载 + 两份文件的交叉校验
│   │   ├── platform.rs        # platform.toml：引脚 id、传感器名、指令模板
│   │   └── policy.rs          # config.toml：全局、分区、联动
│   ├── sensors/
│   │   ├── mod.rs             # SensorHub、Source、SensorSnapshot
│   │   ├── ipmi.rs            # ipmitool sdr 解析（纯函数，可单测）
│   │   └── gpu.rs             # nvml-wrapper，惰性初始化 + feature 开关
│   └── control/
│       ├── mod.rs             # 聚合（max/mean）
│       ├── curve.rs           # 非对称防抖状态机
│       ├── merge.rs           # 冲突取大 → 联动 → 上下限
│       └── engine.rs          # 控制循环、失败策略、优雅退出
├── examples/                  # 可直接用的 platform.toml / config.toml
├── packaging/fanctl.service   # systemd 单元模板
└── tests/                     # 集成测试（配置校验、控制逻辑、生命周期）
```

分层规则一句话：**`control` 只做决定，`actuator` 只做执行，`config` 只有数据。**
因此 `control/` 的全部逻辑都能在没有主板、没有 BMC 的机器上测试。

---

## 3. 配置

### 3.1 `platform.toml` — 硬件适配层

| 字段 | 说明 |
|------|------|
| `[platform].name` | 板名，仅用于日志 |
| `[platform].cmd_template` | `ipmitool` 参数模板，必须含 `{id}` 与 `{pwm}`；`{pwm}` 渲染为 `0xNN` |
| `[platform].ipmitool_path` | 可选，默认 `ipmitool`（走 `$PATH`） |
| `[pins]` | 语义名 → 十六进制引脚 id，必须是 `0x00`–`0xFF` |
| `[sensors]` | 语义名 → `ipmitool sdr` 打印的名字 |

### 3.2 `config.toml` — 业务逻辑层

| 字段 | 说明 |
|------|------|
| `[global].interval_seconds` | 采样周期，≥1 |
| `[global].min_pwm` / `max_pwm` | 全局上下限，`min_pwm` ≥1（不允许停转） |
| `[global].fallback_pwm` | **所有**传感器都读不到时的保底转速，必须落在 `min_pwm..=max_pwm` |
| `[global].shutdown_pwm` | 可选，默认 40；收到 SIGTERM 时写全所有引脚的"安全档" |
| `[global].command_timeout_seconds` | 可选，默认 5；单次 `ipmitool` 的超时 |
| `[[zones]].monitor_sources` | 语义传感器名列表；`"GPU"` 是保留名，代表所有 N 卡取最高 |
| `[[zones]].aggregation` | `max`（默认）或 `mean`，只对**本周期读到值**的源生效 |
| `[[zones]].target_pins` | 该分区驱动的引脚 |
| `[[zones]].default_duty_cycle` | 低于第一档阈值时的待机转速 |
| `[[zones]].debounce_temp` | 单侧防抖，只在**降档**时生效 |
| `[[zones.thresholds]]` | `temperature` 必须升序，`duty_cycle` 取值 1..=100 |
| `[[follows]]` | `pin` 的最终 PWM 不得低于 `min_of`，支持链式、拒绝环 |

校验项（`fanctl check` 与 `run` 启动时都执行，一次列出全部问题）：
TOML 语法与未知字段 → 阈值升序 → `debounce_temp` 必须小于相邻阈值间隔 →
引脚/传感器必须存在 → 阈值与转速范围 → 联动规则无环、无自引用、两端都被某个分区驱动 →
`min_pwm ≤ fallback_pwm ≤ max_pwm`。

> `deny_unknown_fields` 是刻意开的：写错字段名会直接报错，而不是安静地用了默认值。

---

## 4. 控制算法

### 4.1 采集与聚合
`ipmitool sdr` 每条输出取前两列（名字、读数），`45 degrees C` → `45.0`，
非温度（`RPM`、`no reading`、`ns`）记为"没有读数"而不是 0 —— 避免把缺数据的源当成低温。
GPUs 走 NVML，句柄惰性初始化：没装卡/驱动在重启时下个周期会自动重试。

聚合只统计"本周期真有值"的源：`mean` 不会因为没插显卡被拖到 0 ℃。

### 4.2 非对称防抖（快升慢降）
不引入滑动滤波，只用单侧迟滞，档位 `n` 对应第 `n` 个阈值：

| 方向 | 条件 | 动作 |
|------|------|------|
| 升档 | `温度 >= 阈值` | **立即**切档，可一次跳多档 |
| 降档 | `温度 <= 当前档阈值 - debounce_temp` | 降一档 |
| 跌破第一档 | `温度 <= 第一档阈值 - debounce_temp` | 直接回 `default_duty_cycle` |
| 死区内 | 以上都不满足 | 保持当前 PWM 不变 |

以 `55/70/85 → 40/60/100 %`、`debounce_temp=2` 为例：70 ℃ 升到 60 %，要在
**68 ℃ 才降回 40 %**；但 71 ℃ 会立刻升回 60 %。升温不等，降温不抖。

### 4.3 合并与兜底（顺序固定）
1. 各分区算出目标引脚 → **同一引脚取最大值**（绝不覆盖降速）；
2. `follows` 求不动点（与声明顺序无关）→ `Pwm(A) = max(Pwm(A), Pwm(B))`；
3. 夹到 `[min_pwm, max_pwm]`；
4. 只对"与上次成功下发值不同"的引脚发指令（BMC 不会被每 10 秒刷一遍）。

### 4.4 失败策略

| 情况 | 反应 |
|------|------|
| 某个分区读不到数据 | 保持该分区上一次的转速（不降速） |
| 该分区从没读到过数据 | 用 `fallback_pwm` |
| 一个传感器都读不到 | 全部引脚 → `fallback_pwm` |
| `ipmitool` 写失败 / 超时 | 记 Error，**下个周期重试**，不退出、不更新"已下发值" |
| BMC 挂住不回 | 单次调用有超时，控制循环不会被卡死 |
| SIGINT / SIGTERM | 全部引脚 → `shutdown_pwm`，然后正常退出 |

---

## 5. 安全须知

1. **先 `check`，再 `--dry-run`，最后才正式跑。** `check` 会真跑一次 `ipmitool sdr`，
   确认 `[sensors]` 里的名字在这块板子上真的存在。
2. **引脚 id 只做了语法校验**。要确证 `0x00` 就是 CPU_FAN，只能实际写一次再听声音/看转速；
   `check` 只能按名称猜（猜不到会给 `[warn]`，不是错误）。第一次正式跑建议单独盯一次
   `journalctl -u fanctl -f`，确认"改的是哪把扇子"。
3. `min_pwm ≥ 1` 是硬约束，配置里写 0 会直接报错。
4. 进程退出时 BMC 保留最后一次写入的值，所以 `shutdown_pwm`（默认 40 %）比"什么都留不下"重要；
   `TimeoutStopSec` 必须大于一次 `ipmitool` 超时，否则 systemd 会在优雅退出做到一半时 SIGKILL。
5. `ipmitool` 需要 root（或 udev 规则 + `CAP_SYS_RAWIO`）；unit 里刻意**没有**加
   `PrivateDevices=yes` 这类会屏蔽 `/dev/ipmi*` 的选项。
6. BIOS 里若把风扇策略设成 "Full Speed" 或某些 BMC 会在重启后重置 PWM，
   需要相应的 `ipmi` 初始化或接受"重启后由 daemon 下一个周期纠正"。

---

## 6. 已知限制 / 后续可做

- **没有 SIGHUP 热重载**：改配置需 `systemctl restart`（有意为之，配置错误时不会带着半套参数跑）。
- **超时的子进程不 kill**：`ipmitool` 超时后辅助线程继续等它结束，极端情况下会多留一个进程；
  换来的是不会误杀、不会死锁。要彻底解决需引入 `process-group` + `kill`。
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

- Edition 2021，声明 MSRV 1.75（Ubuntu 24.04 自带的 rustc 即可）。`Cargo.lock` 入库，保证部署可复现。
- `#![forbid(unsafe_code)]`；`[lints.clippy] all = "warn"`。
- 所有需要硬件才能验证的东西都被推到边界：`ipmi::parse_sdr`、`curve`、`merge` 全是纯函数，
  测试不依赖 BMC、不依赖 GPU。
