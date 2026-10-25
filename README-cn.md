# fanctl

[English](README.md) · **简体中文**

BMC 自带风扇策略不可用的服务器主板，用它做温控：读温度，按分区曲线换算成占空比，
通过 IPMI raw 写回 BMC。

## 特性

- **动态温控**：每个分区一条阶梯曲线。升温立即跳档，可一次跳多档；降温要越过
  `debounce_celsius` 才落档。[设计](docs/design-cn.md#曲线)
- **主板解耦**：硬件在 `platform.toml`，策略在 `config.toml`，两者成对校验。
  [主板](docs/platform-cn.md) · [配置](docs/config-cn.md)
- **分区**：一个分区可接任意多个输入——某个 BMC 传感器、某张显卡，或全部显卡的最高值。
  [设计](docs/design-cn.md#聚合)
- **联动**：同一引脚被多个分区引用时取最大占空比；`[[follows]]` 表达引脚之间的固定约束。
  [设计](docs/design-cn.md#合并)
- **IPMI raw**：只读分区引用到的传感器，一个周期只起一个 `ipmitool` 进程。
  [设计](docs/design-cn.md#读取)
- **失败策略**：分区读不到数据时保持上一档，之后降到 `fallback_duty_cycle`；BMC 未应答的
  写入下个周期重发。[设计](docs/design-cn.md#读数缺失时的行为)
- **转速回读**：转速写进日志，风扇停转会报警；曲线不读转速。
  [设计](docs/design-cn.md#每周期日志)

## 已验证的主板

| 主板 | 说明 |
| --- | --- |
| TYAN S8030GM2NE | 4 个 PWM 通道，传感器编号 0x60–0x64，转速系数 138 RPM/计数 |
| 具备 OEM raw 风扇指令的其他主板 | 需自备 `platform.toml`；命令模板与传感器编号因厂商而异 |

字段说明、查编号的方法和新增主板的步骤见 [docs/platform-cn.md](docs/platform-cn.md)。

## 依赖

- Linux + systemd。
- `ipmitool` ≥ 1.8，且能打开 `/dev/ipmi0`（root，或授予 `CAP_SYS_RAWIO` 的 udev 规则）。
- BMC 提供可写的 raw PWM 指令。
- 编译需要 Rust 1.84 及以上（`resolver = "3"` 的下限）。`Cargo.lock` 已入库。
- 可选：NVIDIA 驱动与 NVML，用于读取显卡温度；没有驱动时用 `--no-default-features` 编译。

## 安装

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

`examples/` 是 S8030 的参考配置。换主板需要另写一份 `platform.toml`
（[docs/platform-cn.md](docs/platform-cn.md)）和配套的 `config.toml`
（[docs/config-cn.md](docs/config-cn.md)）。

## 使用

```bash
# 校验两份配置并对硬件做一次探测：核对传感器编号、交叉验证 raw 读数、
# 列出显卡、做一次批量读取。不写任何 PWM。
sudo fanctl check

# 跑完整控制循环，但不发 PWM 指令。
sudo fanctl run --dry-run -v

# 前台运行，由 systemd 托管。
sudo fanctl run
```

正常退出返回 0，配置或硬件错误返回 1。日志格式、限流阈值与退出时的复位行为见
[docs/design-cn.md](docs/design-cn.md#每周期日志)。

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

代码大部分由 AI agent 完成，经测试用例与维护者审核。

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
npx markdownlint-cli2 "**/*.md"
```

硬件相关常量一律放 `platform.toml`，不要写进 `src/`。新增 GPU 采集后端的位置是
`src/sensors.rs` 中的 `Source` 枚举；如果显卡温度已经作为 IPMI 传感器发布，则不需要新
代码，只加 `[sensors]` 条目。

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE)。
