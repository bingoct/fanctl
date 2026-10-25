# fanctl

[English](README.md) · **简体中文**

服务器风扇控制守护进程，针对"BMC 自带的 Smart Fan 只有吵和热两档"这类主板。

温度直接从 BMC 经 IPMI 读（NVIDIA 显卡走 NVML），按分区曲线换算成占空比，
再用 `ipmitool raw` 写回。它为一台泰安 S8030 而写 —— 那块板的 Smart Fan 不是
起飞就是慢炖 —— 但控制链路里没有一行代码知道这件事：每一个十六进制常量、
传感器编号和指令模板都在 `platform.toml` 里。

```
cycle temps CPU_Tctl=23.0 GPU0=28.0 GPU1=26.0 | fan CPU_FAN=690 SYS_FAN_1=966 | duty cpu=30 intake=35
```

## 特色

**动态温控，不是固定档位表。** 每个分区一条阶梯曲线 + 非对称迟滞：升温立即
切档（可一次跳多档），降温要先越过下一档 `debounce_celsius` 才落档。热了马上反应，
噪音慢一点。见 [设计](docs/design-cn.md#曲线)。

**主板是配置文件，不是分支。** 引脚 id、传感器编号、`ipmitool` 参数模板、转速
换算系数全在 `platform.toml`。Rust 源码里一个十六进制常量都没有。换一块板是换一个
文件，不是换一个分支 —— 见 [平台支持](docs/platform-cn.md)。

**分区。** 一个分区回答一个问题（"这几把扇子该多快"），输入可以是任意多个传感器：
CPU、机箱空气、某一张显卡、一组的平均值。一个引脚被多个分区认领时永远取最大值，
`[[follows]]` 用来表达"前面板不得比冷排慢"这种约束，而不是堆一堆条件分支。
见 [设计](docs/design-cn.md#合并)。

**采集走 IPMI raw，不走 `ipmitool sdr`。** 整张 SDR 表每轮 2.9 秒，因为每一行都是
一次往返。fanctl 只读分区真正引用的传感器，全部塞进一个 `ipmitool exec`，自己解析
BMC 的原始应答：一个读数 88 ms，七个 183 ms。写入同样批处理 —— 四个引脚 135 ms，
逐个发是 419 ms。见 [开销](docs/cost-cn.md)。

**Rust，一个小二进制。** 1.44 MiB，除 libc 和 libgcc_s 之外静态链接，无运行时，
2 个线程，不加载 NVIDIA 驱动时常驻 8 MB。`panic = "abort"`、`#![forbid(unsafe_code)]`、
生产代码零 `unwrap`。

状态：Linux 上 122 个测试、`clippy -D warnings` 与 `rustfmt` 全绿。在 TYAN
S8030GM2NE（EPYC 7413 + 3 张显卡）上的验收覆盖了：对真机跑 `check`、5 分钟 dry-run、
10 分钟正式控制、`SIGTERM` 复位；此后一直以 systemd 服务形式运行。把它指向你在意的
机器之前，请先读 [安全与边界](docs/design-cn.md#安全与边界)。

## 支持平台

| 主板 | 状态 | 说明 |
|---|---|---|
| TYAN S8030GM2NE | 已实测 | CPU_FAN + 3 个 SYS_FAN 通道，转速系数 138 RPM/计数，`raw 0x2e 0x44 0xfd 0x19 0x00 <ch> 0x01 <duty>` |
| 其他有 OEM raw 风扇指令的主板 | 理论上可用 | 需要一份 `platform.toml`；指令模板和传感器编号各家不同 |

这个程序说的是通用 IPMI —— 它猜不出来的是厂商私有的 PWM 指令。如果你的板子认
`raw 0x2e 0x44 …`（多数带 Smart Fan 和手动覆盖的 AST2500/AST2600 板子都认），
那就只是一次配置工作，[docs/platform-cn.md](docs/platform-cn.md) 把流程写清楚了：
找传感器编号、映射通道、用闭环证明哪个头是哪把扇子。第一次写入之前 `fanctl check`
会先对着活着的 BMC 验证一遍映射。

## 依赖

- Linux + systemd。没有提供其他 init 集成。
- `ipmitool` ≥ 1.8，且能访问 `/dev/ipmi0`（root，或给 `CAP_SYS_RAWIO` 的 udev 规则）。
- 一块接受 raw PWM 指令的 BMC。
- 编译需要 Rust 1.84+（`resolver = "3"` 的下限）。`Cargo.lock` 入库。
- 可选：NVIDIA 驱动，用于读显卡温度。没有驱动的机器用 `--no-default-features` 编译。

## 安装

还没有打包，手动编译安装：

```bash
cargo build --release
cargo build --release --no-default-features    # 机器上没有 NVIDIA 驱动时

sudo install -Dm755 target/release/fanctl          /usr/local/bin/fanctl
sudo install -Dm644 examples/platform.toml         /etc/fanctl/platform.toml
sudo install -Dm644 examples/config.toml           /etc/fanctl/config.toml
```

`examples/` 里是那块已实测主板的参考配置 —— 从它开始，然后读
[配置](docs/config-cn.md) 按你的硬件改曲线。两份文件是成对校验的，
`fanctl check` 一次列出全部问题，而不是第一个。

## 使用

三步，按顺序来，每一步都是上一步的超集。

```bash
# 1. 校验两份配置并探测硬件：读 platform.toml 里写的传感器编号、
#    和 `ipmitool sdr` 交叉核对、列显卡、做一次批量 raw 读。不写任何值。
sudo fanctl check

# 2. 跑控制循环但不发任何 PWM 指令。每周期把"本来会写什么"打进日志。
#    这一步抓的是"曲线写错了"，配置校验器抓不到这个。
sudo fanctl run --dry-run -v

# 3. 正式运行，前台跑，交给 systemd 托管。
sudo fanctl run
```

systemd：

```bash
sudo install -Dm644 packaging/fanctl.service /etc/systemd/system/fanctl.service
sudo systemctl daemon-reload
sudo systemctl enable --now fanctl
journalctl -u fanctl -f
```

unit 是按 root + 前台进程写的：没有 `PIDFile=`，没有 `PrivateDevices=`（那会
藏掉 `/dev/ipmi*`），`TimeoutStopSec=120s` 用来兜住退出时的复位写入。

**日志就是接口。** 每周期一行，按变化限流：

| 发生了什么 | 级别 |
|---|---|
| 有温度变化 ≥ 3 ℃、转速变化超过 2 个计数、或有传感器出现/消失 | INFO |
| 什么都没变，距上次 INFO 不到 5 分钟 | DEBUG |
| 什么都没变，但满 5 分钟了 | INFO |
| 被驱动的风扇从转变成不转 | WARN |

所以空载机器一天 288 行，负载一动就是 10 秒一行，而风扇坏了从不会沉默。`-v` 把级别
降到 DEBUG（每周期都打），`-vv` 到 TRACE。正常退出退出码 0，配置或硬件错误为 1。

画温度曲线：

```bash
journalctl -u fanctl -o cat | grep '^cycle ' > cycles.log
```

## 配置

两份文件，刻意分开：

- `platform.toml` —— 硬件是什么：引脚、传感器编号、怎么跟它说话。
  → [docs/platform-cn.md](docs/platform-cn.md)
- `config.toml` —— 策略是什么：分区、曲线、联动、兜底。
  → [docs/config-cn.md](docs/config-cn.md)

两份都在启动时和 `fanctl check` 里校验；未知字段直接报错，而不是安静地忽略拼写
错误。名字一律大小写不敏感。

## 设计

聚合方式、曲线、多个分区抢同一个引脚、传感器读不到了会怎样 —— 都在
[docs/design-cn.md](docs/design-cn.md)。一个周期的实测开销和二进制里那些字节都
花在哪，在 [docs/cost-cn.md](docs/cost-cn.md)。

## 贡献

**最有用的贡献是另一块主板。** [docs/platform-cn.md](docs/platform-cn.md) 写清了每个
字段的含义和找编号的流程；一个能用的 `platform.toml`（外加一份演示它的 config）
就能把这个只能跑一块板的工具变成通用工具。风扇控制不是 raw 厂商指令的板子 ——
Dell iDRAC、HPE iLO、用 Redfish 风扇策略的超微 —— 需要换一个执行后端，那是一个很小
且隔离得很干净的模块。

**第二有用的贡献是另一种 GPU 采集后端。** 现在温度来自 NVML，只覆盖 NVIDIA。AMD
（ROCm SMI）和 Intel（`xe`/`i915` hwmon）都能通过别的接口给出每卡温度；而如果一块
板子把显卡温度当成 IPMI 传感器发布，那连新代码都不用写 —— 加几条 `[sensors]` 就行。
新的后端插在 `src/sensors.rs` 的 `Source` 枚举上。

提交补丁之前跑：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
`cargo test`。硬件知识不要进 `src/` —— 凡是硬件相关的常量都属于 `platform.toml`。
报控制问题时请附上 `fanctl check` 的输出和几行 `cycle` 日志；那行日志存在的全部意义
就是让远程诊断成为可能。

## 这个软件是怎么写出来的

**大部分代码由一个 AI agent 敲出来**，设计由一个维护者定，每一条硬件事实都在机器上
亲自验证过：哪个头对应哪个通道、一把扇子在多少占空比下才开始转、`raw 0x04 0x2d`
返回的是转速计数而不是 RPM、水泵在 90 % 以上基本饱和。这些事实之所以写在注释和文档里，
是因为它们被量过，而不是因为被假定 —— 其中有几条最初的假设就是错的，被硬件纠正了。

这对你意味着什么：代码很小（约 5 000 行，其中控制链路只有几百行），测试不需要任何硬件
就能跑，里面没有魔法。同时也意味着这个项目的运维经验只有一个人、一块板那么多。
第一次部署请按它当时被对待的方式对待 —— `check`，然后 `--dry-run`，然后盯着它跑。

## 许可证

GPL-3.0-or-later。见 [LICENSE](LICENSE)。
