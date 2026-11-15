# 配置

[English](config.md) · **简体中文**

`config.toml` 描述策略：某个温度下风扇转多快。它不含硬件知识；引用的引脚名与
传感器名都通过 [`platform.toml`](platform-cn.md) 解析，不区分大小写。

相关：[设计](design-cn.md) · [主板](platform-cn.md) · [开销](cost-cn.md)

## 结构

```toml
[global]
interval_seconds = 10
min_duty_cycle = 30
max_duty_cycle = 100
default_duty_cycle = 30
fallback_duty_cycle = 60

[[zones]]
name = "cpu"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]

[[zones.thresholds]]
temperature = 50
duty_cycle = 35

[[zones.thresholds]]
temperature = 70
duty_cycle = 60

[[zones.thresholds]]
temperature = 84
duty_cycle = 100
```

每个分区至少一档阈值：曲线就是策略，其余字段用于联动、边界与失效处理。
未知字段报错，拼写错误不会退回默认值。

## `[global]`

| 字段 | 必填 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `interval_seconds` | 是 | — | 采样周期，≥ 1。周期包含本轮工作耗时：10 秒周期、读取 130 ms，休眠 9.87 秒。 |
| `min_duty_cycle` | 是 | — | 占空比下限，1–100。取 `1`：停转服务器风扇不是受支持的状态。 |
| `max_duty_cycle` | 是 | — | 占空比上限，≤ 100。 |
| `default_duty_cycle` | 是 | — | 待机值。低于第一档时使用，退出时写回。不得高于第一档。 |
| `fallback_duty_cycle` | 是 | — | 分区完全读不到数据时的值。不得低于本分区的 `default_duty_cycle`。 |
| `debounce_celsius` | 否 | `2.0` | 降档迟滞，必须小于到下一档的间隔。 |
| `aggregation` | 否 | `max` | 分区输入的聚合方式，`max` 或 `mean`。 |
| `fallback_after_cycles` | 否 | `2` | 连续多少个周期无读数后降到 `fallback_duty_cycle`。≥ 1。 |
| `command_timeout_seconds` | 否 | `10` | 单次 `ipmitool` 调用的超时。≥ 1。 |

两份文件合计只有五个可省略字段：`debounce_celsius`、`aggregation`、
`fallback_after_cycles`、`command_timeout_seconds`，以及 `platform.toml` 的
`ipmitool_path`。其余字段必填，或从 `[global]` 继承到分区。

## `[[zones]]`

一个分区把一组传感器映射到一组引脚。

| 字段 | 必填 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `name` | 是 | — | 唯一名字（不区分大小写），出现在日志与周期行里。 |
| `monitor_sources` | 是 | — | 非空。每项是 `[sensors]` 的键，或 `GPU` / `GPU<n>`。 |
| `target_pins` | 是 | — | 非空。每项必须在 `[pins]` 中声明。 |
| `thresholds` | 是 | — | 至少一档，`temperature` 严格递增。 |
| `aggregation` | 否 | 继承 `[global]` | `max` 或 `mean`，只统计本轮有效上报的输入。 |
| `default_duty_cycle` | 否 | 继承 `[global]` | 1–100，不高于第一档。 |
| `fallback_duty_cycle` | 否 | 继承 `[global]` | 1–100，不低于本分区的 `default_duty_cycle`。 |
| `fallback_after_cycles` | 否 | 继承 `[global]` | ≥ 1。 |
| `debounce_celsius` | 否 | 继承 `[global]` | ≥ 0，小于到下一档的间隔。 |

### `[[zones.thresholds]]`

| 字段 | 必填 | 含义 |
| --- | --- | --- |
| `temperature` | 是 | 摄氏度，严格大于上一档。 |
| `duty_cycle` | 是 | 1–100。不要求递增，但递减曲线几乎总是错误。 |

每档从自身温度生效到下一档；高于最后一档保持最后一档，低于第一档用
`default_duty_cycle`。

### 继承

五个字段是共享策略：`aggregation`、`default_duty_cycle`、`debounce_celsius`、
`fallback_duty_cycle`、`fallback_after_cycles`。写在 `[global]` 是所有分区的默认值，
写在 `[[zones]]` 内覆盖该分区。继承在加载期展开一次，控制循环不回查全局值。

水泵同时覆盖两个占空比：

```toml
[global]
default_duty_cycle = 30
fallback_duty_cycle = 60

[[zones]]
name = "pump"
monitor_sources = ["Water_Inlet"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100    # 从不低速运行
fallback_duty_cycle = 100   # 读不到数据时也不停转

[[zones.thresholds]]
temperature = 20
duty_cycle = 100
```

### 显卡输入

`monitor_sources` 接受三类条目：

| 条目 | 读取内容 |
| --- | --- |
| `"CPU_Tctl"` 等 `[sensors]` 的键 | 该 BMC 传感器 |
| `"GPU"` | NVML 可见的全部 N 卡中的最高温度 |
| `"GPU0"`、`"GPU1"` 等 | 按 NVML 序号指定的单张显卡 |

`GPU` 是保留名，`[sensors]` 不得定义。序号按驱动枚举顺序排列，不更换显卡就跨重启
稳定。`fanctl check` 输出每张显卡的序号、名称与温度。

`mean` 只对本轮有效上报的输入取平均，缺失的显卡不会把分区拉向 0 ℃。

## `[[follows]]`

引脚之间的联动，在每个分区完成决策后应用：

```toml
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

含义是"前面板不得比冷排慢"。两端都必须被分区驱动；`pin` 不得引用自身；规则成环
在加载期报错。多条规则迭代到不动点，结果与书写顺序无关。见[设计](design-cn.md#合并)。

## 名字匹配

名字比较不区分大小写：`cpu_fan`、`CPU_FAN`、`Cpu_Fan` 是同一个引脚；`"gpu"` 即
`"GPU"`，`"gpu1"` 即 `"GPU1"`。

加载期把 `config.toml` 的每个引用改写为 `platform.toml` 声明的写法，之后才做精确
比较，`check` 与 `run` 因此不会对同一个引脚给出不同判断。`platform.toml` 中仅大小写
不同的两个键会被拒绝。SDR 名字额外忽略分隔符：`CPU_Tctl_Value`、`CPU Tctl Value`、
`cpu-tctl-value` 是同一个传感器。

## 校验

`fanctl check` 与 `fanctl run` 启动时用同一个校验器，一次列出全部问题：

- TOML 语法与未知字段；
- 阈值严格递增；`debounce_celsius` 小于相邻两档的间隔；
- 引用的引脚与传感器都存在；`[sensors]` 未占用 `GPU` 名字；
- `1 <= min_duty_cycle <= max_duty_cycle <= 100`；
- 每个分区的 `default_duty_cycle` 在 `[min, max]` 内且不高于第一档；
- 每个分区的 `fallback_duty_cycle` 在 `[min, max]` 内且不低于自身
  `default_duty_cycle`；
- `fallback_after_cycles >= 1`、`command_timeout_seconds >= 1`、
  `interval_seconds >= 1`；
- `follows`：两端均被驱动，无自引用，无环；
- 仅大小写不同的名字冲突。

`GPU<n>` 是否存在是运行期事实。`check` 打印实际找到的显卡；引用不存在的显卡时，
该分区按"无读数"路径处理（先保持，后降档），不导致启动失败。

## 示例

`examples/config.toml` 是带注释的完整文件，可作起点。下面是参考机器上的策略：
水冷 EPYC，两张 4080 共用顶部冷排，一张风冷 3080。

```toml
[global]
interval_seconds = 10
min_duty_cycle = 30
max_duty_cycle = 100
default_duty_cycle = 35
fallback_duty_cycle = 60
fallback_after_cycles = 2
debounce_celsius = 2
aggregation = "max"

# Pump: flat out, always - including when nothing can be read, and including
# the shutdown path, where it is written back at 100 % instead of standby.
[[zones]]
name = "pump"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100
fallback_duty_cycle = 100
[[zones.thresholds]]
temperature = 0
duty_cycle = 100

# CPU heatsink and rear exhaust, on package temperature.
[[zones]]
name = "cpu"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
[[zones.thresholds]]
temperature = 45
duty_cycle = 40
[[zones.thresholds]]
temperature = 55
duty_cycle = 50
[[zones.thresholds]]
temperature = 65
duty_cycle = 60
[[zones.thresholds]]
temperature = 72
duty_cycle = 70
[[zones.thresholds]]
temperature = 78
duty_cycle = 80
[[zones.thresholds]]
temperature = 84
duty_cycle = 100

# The radiator over the two 4080s. The 3080 is deliberately absent: it has its
# own fans, and binding this zone to every GPU would run the radiator for heat
# it is not cooling. The curve stops at 70 % because that is the case's
# positive-pressure ceiling, not the radiator's thermal one.
[[zones]]
name = "radiator"
monitor_sources = ["GPU1", "GPU2"]
target_pins = ["SYS_FAN_2"]
default_duty_cycle = 30
[[zones.thresholds]]
temperature = 50
duty_cycle = 35
[[zones.thresholds]]
temperature = 58
duty_cycle = 45
[[zones.thresholds]]
temperature = 68
duty_cycle = 55
[[zones.thresholds]]
temperature = 74
duty_cycle = 60
[[zones.thresholds]]
temperature = 79
duty_cycle = 70

# Front panel: every heat source in the case, including the self-fanned 3080.
# The top radiator pulls case air, so the front fans are the inlet for the whole
# machine; watching only the two 4080 would let the 3080 recirculate its own
# exhaust.
[[zones]]
name = "intake"
monitor_sources = ["CPU_Tctl", "GPU"]
target_pins = ["SYS_FAN_1"]
default_duty_cycle = 35
[[zones.thresholds]]
temperature = 50
duty_cycle = 35
[[zones.thresholds]]
temperature = 58
duty_cycle = 45
[[zones.thresholds]]
temperature = 66
duty_cycle = 60
[[zones.thresholds]]
temperature = 74
duty_cycle = 80
[[zones.thresholds]]
temperature = 80
duty_cycle = 100

# Positive pressure: the intake is never slower than the radiator.
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

每档占空比都是 5 % 的整数倍：BMC 量化 PWM，该主板上 25 %、28 %、30 % 都是 828 RPM。
每档写入后都回读过转速。
