# 支持平台

[English](platform.md) · **简体中文**

`platform.toml` 描述的是机器。它是唯一知道十六进制数的文件，也是把 fanctl 挪到另一块
板子上唯一需要替换的文件。`config.toml` —— 策略 —— 原样不动。

相关：[配置](config-cn.md) · [设计](design-cn.md) · [开销](cost-cn.md)

## 已实测主板

| 主板 | BMC | 状态 | 细节 |
|---|---|---|---|
| TYAN S8030GM2NE | AST2500 | 已在真机验证 | 4 个 PWM 通道在用，传感器 0x60–0x64，转速系数 138 RPM/计数 |
| 其他有 OEM raw 风扇指令的板子 | — | 理论可用 | 需要一份新的 `platform.toml`，流程见[添加一块主板](#添加一块主板) |

"已实测"的意思是：`fanctl check` 对着活的 BMC 通过、每个传感器编号都用
`ipmitool sdr get` 交叉核对过、通道到头子的映射是"写一个通道、看哪把扇子变转速"
确认的、并且已经以 systemd 服务形式长期运行。

不支持、也离支持很远的：Dell iDRAC、HPE iLO、以及用 Redfish 风扇策略的超微主板。
它们的风扇控制走 Web API 而不是 raw IPMI 指令，需要另一个执行后端。那个接口很小
（见[贡献](../README-cn.md#贡献)），但还没人写。

## 文件结构

```toml
[platform]
name = "TYAN S8030GM2NE"
cmd_template = "raw 0x2e 0x44 0xfd 0x19 0x00 {id} 0x01 {pwm}"
rpm_per_count = 138

[pins]
"CPU_FAN"   = "0x00"
"SYS_FAN_1" = "0x02"
"SYS_FAN_2" = "0x03"
"SYS_FAN_4" = "0x05"

[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }

[tachometers]
"CPU_FAN" = { sdr = "CPU_FAN", id = "0x60" }
```

### `[platform]`

| 字段 | 必填 | 默认值 | 含义 |
|---|---|---|---|
| `name` | 是 | — | 板名。只进日志。 |
| `cmd_template` | 是 | — | `ipmitool` 参数，必须含 `{id}` 和 `{pwm}`；`{pwm}` 渲染成 `0xNN`。 |
| `ipmitool_path` | 否 | `ipmitool`（走 `$PATH`） | systemd 下 `$PATH` 很短，建议写绝对路径。 |
| `rpm_per_count` | `[tachometers]` 非空时 | — | 一个 raw 转速计数等于多少 RPM，见[转速计](#转速计)。 |

`cmd_template` 是厂商私有指令。每次写入、每个引脚展开一次，程序的其他部分不知道
它是什么意思。

### `[pins]`

逻辑通道名 → 通道 id，两者都是板级知识。

| | |
|---|---|
| 必填 | 至少 1 条 |
| 值 | `0x00`–`0xFF`，带引号 |
| 名字 | 忽略大小写唯一；`config.toml` 用它引用 |

名字随你起（`"CPU_FAN"`、`"front"`、`"pump"`），它是 `config.toml` 唯一使用的句柄。
没有任何分区指向的引脚不会被写、退出时也不会被复位；`check` 会给一条 warn —— 如果你
为了看转速而保留了一个通道声明，这条 warn 就是预期的。

### `[sensors]`

逻辑传感器名 → 怎么读。两半都必填，都会被校验：

```toml
[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
```

- `id` 是 IPMI **传感器编号**，控制循环用它读温度（`raw 0x04 0x2d <id>`）。
- `sdr` 是 `ipmitool sdr` 打印的名字。`check` 用它做诊断、交叉验证 `id`，以及生成
  人看得懂的失败信息。

两个名字不能共用一个编号：其中一个会安静地读到另一个的温度。`GPU`、`GPU0`、`GPU1`……
是保留名，在这里会被拒 —— 它们是 NVIDIA 设备，不是 BMC 传感器（见
[配置](config-cn.md#显卡源)）。

只有被某个分区引用的传感器才会被读。声明一个传感器不花任何代价。

### 转速计

逻辑引脚名 → 同一个头子上的转速传感器。可选。

```toml
[platform]
rpm_per_count = 138

[tachometers]
"SYS_FAN_1" = { sdr = "SYS_FAN_1", id = "0x61" }
```

转速显示在每周期的日志行里，并在风扇停转时告警。它从不影响任何占空比。

这里有两件事很容易搞错：

**编号不是通道。** `SYS_FAN_1` 可能是通过通道 `0x02` 写的、却从传感器 `0x61` 读回来。
PWM 通道和转速传感器是两套编号空间，之间没有任何算术关系；唯一的办法是查。

**`raw 0x04 0x2d` 返回的是计数，不是 RPM。** `ipmitool sdr` 之所以打印 RPM，是它在
本地套用了 SDR 记录里的 M 系数，BMC 并不管。S8030 上每个风扇读数都是 138 的整数倍，
所以 `rpm_per_count = 138` 把计数 7 变成 966 RPM。`check` 会把
`计数 × rpm_per_count` 与 `ipmitool sdr get` 比对，差一个计数以上就判失败 —— 系数写错
会为一个没人测量的风扇印出一个自信又合理的数字。

键必须指向 `[pins]` 里声明过的引脚。如果硬件改了、那个通道不再驱动任何东西（比如把
水泵直接接到大 4pin 上，这是合理的做法），保留引脚声明、删掉那个分区：转速照样能读，
通道只是再也不被写。

## 找编号

`platform.toml` 需要的一切都在机器上。

**传感器名字和编号：**

```bash
sudo ipmitool sdr | grep -i -E 'fan|temp'
sudo ipmitool sdr get CPU_Tctl_Value | head -3     # Sensor ID : CPU_Tctl_Value (0x1)
```

**PWM 通道**没有可发现的办法 —— 通道号到物理头子的映射是厂商知识。如果这块板有能手动
设风扇转速的厂商工具，用它并回读通道；否则找到厂商文档或该主板的论坛帖，然后动手证明
这个映射：

```bash
# 写一个通道，看哪把扇子变转速，然后把值写回去。
sudo ipmitool raw 0x2e 0x44 0xfd 0x19 0x00 0x03 0x01 0x64    # 通道 3 → 100 %
sleep 12
sudo ipmitool sdr type Fan
```

正经的测试要用两个差得很远的占空比，而不是相邻的两个：风扇读数是量化的（这块板一个
计数 138 RPM），30 % 和 35 % 完全可能返回同一个数，而这跟硬件没关系。S8030 上水泵
30 % 读 1104、100 % 读 3036，这就没有歧义。

**转速系数：** 拿一次 raw 读和一次 `sdr get` 对比：

```bash
sudo ipmitool raw 0x04 0x2d 0x61     # 07 c0 c0 00
sudo ipmitool sdr get SYS_FAN_1      # Sensor Reading : 966 (+/- 0) RPM
```

`966 / 7 = 138`。之后 `fanctl check` 会一直帮你盯着。

## 添加一块主板

1. 复制 `examples/platform.toml`，替换 `name`、`cmd_template`、`[pins]`、`[sensors]`。
2. 跑 `sudo fanctl check`。它会逐传感器打印：编号是否存在、`sdr` 名字是否指向这个
   编号、raw 读数和 `ipmitool sdr` 是否一致；然后列出显卡；最后做一次批量读并报告
   耗时。
3. 想要转速进日志，就加 `[tachometers]` 和 `rpm_per_count`。
4. 跑 `fanctl run --dry-run -v`，把日志里的占空比和你手写的曲线对一遍。
5. 这才正式跑，并且头几分钟盯着 `journalctl -u fanctl -f` —— 同时确认变了转速的那个
   头子就是你想改的那个。

如果这块板需要的指令形状 `cmd_template` 表达不了 —— 每通道校验和、每次写入前两步
解锁 —— 那就需要写代码而不是写配置。把指令序列贴到 issue 里；渲染只有一个函数
（`PwmActuator::render`）。

一份能用的 `platform.toml` 加一份演示它的 `config.toml`，是这个项目最需要的那种贡献。
