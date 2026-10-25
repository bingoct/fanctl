# fanctl

[English](README.md) · **简体中文**

服务器主板的风扇温控守护进程。针对 BMC 自带风扇控制不可用、或只能在噪音与过热之间
取舍的主板：从 BMC 读取温度，按分区曲线换算占空比，再写回 BMC。

- 温度来源：BMC 传感器（IPMI raw）、NVIDIA 显卡（NVML）。
- 控制周期由 `interval_seconds` 决定，参考配置取 10 s。一个周期内所有读数合并为一次
  `ipmitool exec`，所有写入合并为一次。
- 主板相关的引脚 id、传感器编号、命令模板、转速系数全部在 `platform.toml` 里，
  Rust 源码中不含十六进制常量。

当前落地点是一台 TYAN S8030GM2NE（EPYC 7413 + 3 张显卡），以 systemd 服务形式长期运行。
换一块主板是换一份 `platform.toml`，不需要改代码。

## 特性

- **动态温控**：每个分区一条阶梯曲线，升温立即跳档（可一次跳多档），降温需越过
  `debounce_celsius` 才落档。
- **主板解耦**：`platform.toml` 描述硬件，`config.toml` 描述策略，两者成对校验。
- **分区**：一个分区的输入可以是任意多个传感器（BMC 传感器、单张显卡、全部显卡的最大值）。
- **联动**：同一引脚被多个分区引用时取最大占空比；`[[follows]]` 表达引脚之间的固定约束。
- **采集走 IPMI raw**：不使用 `ipmitool sdr`，只读分区引用到的传感器，7 个读数一次
  `exec` 用时 183 ms。
- **失败策略**：分区读不到数据时先保持上一档，连续 `fallback_after_cycles` 个周期后降到
  `fallback_duty_cycle`；写 PWM 未获 BMC 应答时记日志并在下个周期重发。
- **Rust 单文件**：1.44 MiB，除 libc 与 libgcc_s 之外静态链接，2 线程，不加载 NVML 时
  常驻内存 8 MB。

## 支持平台

| 主板 | 状态 | 说明 |
| --- | --- | --- |
| TYAN S8030GM2NE | 实机验证 | 4 个 PWM 通道，传感器编号 0x60–0x64，转速系数 138 RPM/计数 |
| 具备 OEM raw 风扇指令的其他主板 | 可用，需自备配置 | 命令模板与传感器编号因厂商而异 |

`[platform]` 的硬件事实只能靠实测获得：PWM 通道号与传感器编号属于两套互不相关的编号，
通道与物理风扇头的对应关系需要用转速闭环确认。[docs/platform-cn.md](docs/platform-cn.md)
给出字段说明、查编号的方法和新增主板的步骤。首次写入前 `fanctl check` 会用
`ipmitool sdr` 交叉核对配置里写的每一个编号。

## 依赖

- Linux + systemd。
- `ipmitool` ≥ 1.8，且能打开 `/dev/ipmi0`（root，或授予 `CAP_SYS_RAWIO` 的 udev 规则）。
- BMC 提供可写的 raw PWM 指令。
- 编译需要 Rust 1.84 及以上（`resolver = "3"` 的下限）。`Cargo.lock` 已入库。
- 可选：NVIDIA 驱动与 NVML，用于读取显卡温度；没有驱动时用 `--no-default-features`
  编译，二进制小 106 KB。

## 安装

目前没有打包，手动编译安装：

```bash
cargo build --release                              # 需要读显卡温度时
cargo build --release --no-default-features        # 无 NVIDIA 驱动时

sudo install -Dm755 target/release/fanctl    /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml   /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml     /etc/fanctl/config.toml

sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
```

`examples/` 是 S8030 的参考配置。改用其他主板时，先按
[docs/platform-cn.md](docs/platform-cn.md) 改写 `platform.toml`，再按
[docs/config-cn.md](docs/config-cn.md) 调整 `config.toml`。

## 使用

```bash
# 校验两份配置，并对硬件做一次探测：核对传感器编号、交叉验证 raw 读数、
# 列出显卡、做一次批量读取。不写任何 PWM。
sudo fanctl check

# 跑完整控制循环但不发 PWM 指令，日志里是每周期本应写入的值。
sudo fanctl run --dry-run -v

# 前台运行，由 systemd 托管。
sudo fanctl run
```

日志每周期一行，按温度、转速、传感器增删的变化限流；`-v` 输出每个周期，`-vv` 输出
TRACE。正常退出返回 0，配置或硬件错误返回 1。日志格式、限流阈值与退出时的复位行为
见 [docs/design-cn.md](docs/design-cn.md#每周期日志)。

导出温度曲线：

```bash
journalctl -u fanctl -o cat | grep '^cycle ' > cycles.log
```

## 配置

| 文件 | 内容 | 文档 |
| --- | --- | --- |
| `/etc/fanctl/platform.toml` | 硬件：引脚 id、传感器编号、命令模板、转速系数 | [docs/platform-cn.md](docs/platform-cn.md) |
| `/etc/fanctl/config.toml` | 策略：分区、曲线、联动、兜底 | [docs/config-cn.md](docs/config-cn.md) |

两份文件在启动时和 `fanctl check` 中校验，未知字段直接报错。名字比较一律不区分大小写。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/design-cn.md](docs/design-cn.md) | 一个周期的处理流程、读取协议、聚合方式、曲线与防抖、分区合并、失败策略、日志、安全边界 |
| [docs/config-cn.md](docs/config-cn.md) | `config.toml` 全部字段与默认值、校验规则、一份完整示例 |
| [docs/platform-cn.md](docs/platform-cn.md) | `platform.toml` 字段、查传感器编号与 PWM 通道的方法、新增主板 |
| [docs/cost-cn.md](docs/cost-cn.md) | 读写耗时实测、二进制体积构成、常驻内存、与同类实现的对比 |

## 开发

提交前在 Linux 上跑完以下三项（`util/process.rs` 的超时与信号处理不跨平台，Windows
上的 `cargo test` 不算数）：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npx markdownlint-cli2 "**/*.md"     # 改动文档时
```

硬件相关常量一律放 `platform.toml`，不要写进 `src/`。新增 GPU 采集后端的位置是
`src/sensors.rs` 中的 `Source` 枚举；如果主板的显卡温度已经作为 IPMI 传感器发布，
则不需要新代码，只加 `[sensors]` 条目。报告控制问题时请附上 `fanctl check` 的输出
和若干行 `cycle` 日志。

本项目大部分代码由 AI agent 完成，设计由维护者确定，硬件事实均在实机上测量：通道与
风扇头的对应关系、各档占空比对应的实际转速、`raw 0x04 0x2d` 返回的是计数值而非 RPM、
水泵的转速饱和点。这些数据写在配置注释与文档中。测试覆盖 124 个用例，`clippy` 与
`rustfmt` 检查通过。运维经验集中在 S8030 这一块主板上，其他主板首次部署请按
`check` → `--dry-run` → 观察的顺序进行。

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE)。
