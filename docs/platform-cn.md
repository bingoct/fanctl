# 主板

[English](platform.md) · **简体中文**

`platform.toml` 描述硬件：它是唯一含十六进制数字的文件，也是换主板时唯一要替换的
文件，`config.toml` 保持不变。

相关：[配置](config-cn.md) · [设计](design-cn.md) · [开销](cost-cn.md)

## 已验证主板

| 主板 | BMC | 说明 |
| --- | --- | --- |
| TYAN S8030GM2NE | AST2500 | 4 个 PWM 通道，传感器 0x60–0x64，转速系数 138 RPM/计数 |

"已验证"指：`fanctl check` 对活 BMC 通过；每个传感器编号与 `ipmitool sdr get`
核对过；通道与风扇头的对应关系由"写单一通道、观察哪把风扇变速"确认；守护进程
已作为 systemd 服务运行过。

Dell iDRAC、HPE iLO、使用 Redfish 风扇策略的超微主板不受支持：它们通过 Web API
而非 raw IPMI 控制风扇，需要另一个执行后端。后端接口很小（`PwmActuator`），目前
没有实现。

## 文件

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
| --- | --- | --- | --- |
| `name` | 是 | — | 主板名称，用于日志。 |
| `cmd_template` | 是 | — | `ipmitool` 参数，必须含 `{id}` 与 `{pwm}`；`{pwm}` 渲染为 `0xNN`。 |
| `ipmitool_path` | 否 | 从 `$PATH` 查找 | systemd 下 `$PATH` 很短，建议写绝对路径。 |
| `rpm_per_count` | `[tachometers]` 非空时必填 | — | 每个转速计数对应的 RPM，见[转速计](#转速计)。 |

`cmd_template` 是厂商私有指令，每次写入按引脚展开一次，程序其余部分不解释它。

### `[pins]`

逻辑通道名到通道 id，两者都由主板决定。

| | |
| --- | --- |
| 必填 | 至少一个 |
| 取值 | `0x00`–`0xFF`，带引号 |
| 名字 | 不区分大小写唯一，供 `config.toml` 引用 |

名字任意取（`"CPU_FAN"`、`"front"`、`"pump"`），是 `config.toml` 唯一使用的句柄。
没有分区引用的引脚不写入、退出时也不复位；`check` 对其给出 `[warn]`，只为读转速
而声明的通道属于预期情况。

### `[sensors]`

逻辑传感器名到读取方式，两部分都必填、都会被校验：

```toml
[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
```

- `id` 是 IPMI 传感器编号，控制循环用它（`raw 0x04 0x2d <id>`）。
- `sdr` 是 `ipmitool sdr` 输出的名字，`check` 用它做诊断、交叉验证 `id`、生成
  可读的错误信息。

两个名字不得共用同一个编号——其中一个会安静地读到另一个的温度。`GPU`、`GPU0`、
`GPU1` 是保留名，在此被拒绝：它们是 NVIDIA 设备，不是 BMC 传感器，见
[配置](config-cn.md#显卡输入)。

只有被分区引用的传感器才会被读取，声明不用的传感器没有代价。

### 转速计

逻辑引脚名到同一风扇头上的转速传感器，可选。

```toml
[platform]
rpm_per_count = 138

[tachometers]
"SYS_FAN_1" = { sdr = "SYS_FAN_1", id = "0x61" }
```

转速出现在周期日志里，风扇停转产生一条警告；转速不参与占空比计算。

**传感器编号不是通道号。** `SYS_FAN_1` 可能从通道 `0x02` 写入、从传感器 `0x61`
读回。PWM 通道与传感器编号是两套编号空间，没有换算关系，只能逐个查。

**`raw 0x04 0x2d` 返回计数，不是 RPM。** `ipmitool sdr` 输出 RPM 是因为它在本机
套用了 SDR 记录的 M 系数，BMC 不套。S8030 上全部风扇读数都是 138 的整数倍，
`rpm_per_count = 138` 把计数 7 换算成 966 RPM。`check` 比较 `计数 × rpm_per_count`
与 `ipmitool sdr get`，相差超过三个计数即失败。

这里的键必须是 `[pins]` 声明过的引脚。硬件改动使通道不再驱动风扇时——例如水泵
改接大 4 pin 直供——保留引脚声明、删除对应分区：转速计继续可用，通道不会再被写入。

## 查找编号

`platform.toml` 需要的信息都能在目标机上取到。

**传感器名字与编号：**

```bash
sudo ipmitool sdr | grep -i -E 'fan|temp'
sudo ipmitool sdr get CPU_Tctl_Value | head -3     # Sensor ID : CPU_Tctl_Value (0x1)
```

**PWM 通道无法自动发现**，通道号与物理风扇头的对应关系是厂商信息。主板有可手动
设速的厂商工具时，用它设速并读回通道；否则从厂商文档或该主板的论坛帖找指令，
再手工证明对应关系：

```bash
# 写一个通道，观察哪把风扇变化，然后把值改回去。
sudo ipmitool raw 0x2e 0x44 0xfd 0x19 0x00 0x03 0x01 0x64    # 通道 3 -> 100 %
sleep 12
sudo ipmitool sdr type Fan
```

对比用的两个占空比要拉开距离：转速读数有量化（本板每计数 138 RPM），30 % 与 35 %
可能给出同一个数。S8030 上水泵 30 % 读 1104，100 % 读 3036。

**转速系数**：比较一次 raw 读数与一次 `sdr get`：

```bash
sudo ipmitool raw 0x04 0x2d 0x61     # 07 c0 c0 00
sudo ipmitool sdr get SYS_FAN_1      # Sensor Reading : 966 (+/- 0) RPM
```

`966 / 7 = 138`。此后每次 `fanctl check` 都会复核这个值。

## 新增主板

1. 复制 `examples/platform.toml`，替换 `name`、`cmd_template`、`[pins]`、`[sensors]`。
2. 运行 `sudo fanctl check`。它逐个传感器输出编号是否存在、`sdr` 是否指向该编号、
   raw 读数与 `ipmitool sdr` 是否一致，随后列出显卡、做一次带计时的批量读取。
3. 需要日志含转速时，加入 `[tachometers]` 与 `rpm_per_count`。
4. 运行 `fanctl run --dry-run -v`，对照曲线核对日志中的占空比。
5. 确认无误再正式运行，前几分钟用 `journalctl -u fanctl -f` 观察，确认变化的
   风扇头正是预期的那一个。

主板需要的指令形式无法用 `cmd_template` 表达时——带逐通道校验和、每次写入前
两步解锁——执行层需要代码而不是配置。请带着指令序列开 issue；渲染逻辑在
`PwmActuator::render` 一处。
