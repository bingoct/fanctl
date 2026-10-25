# 配置

[English](config.md) · **简体中文**

`config.toml` 是策略：某个温度下风扇应该多快。它不含任何硬件知识 —— 里面提到的每个
引脚、传感器名字都通过 [`platform.toml`](platform-cn.md) 解析，且大小写不敏感。

相关：[设计](design-cn.md) · [平台支持](platform-cn.md) · [开销](cost-cn.md)

## 形状

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

一个分区至少要有 1 条阈值；曲线就是全部策略。剩下的都是联动、上下限和失败处理。

未知字段直接报错，不是警告 —— 字段名拼错不能安静地落回默认值。

## `[global]`

| 字段 | 必填 | 默认值 | 说明 |
|---|---|---|---|
| `interval_seconds` | 是 | — | 采样**周期**，≥ 1。周期**含**本轮耗时：10 秒周期里读用掉 130 ms 就睡 9.87 秒，不是再额外睡 10 秒。 |
| `min_duty_cycle` | 是 | — | 硬下限，1–100。写 1 是因为"服务器风扇停转"不是支持的状态。 |
| `max_duty_cycle` | 是 | — | 硬上限，≤ 100。 |
| `default_duty_cycle` | 是 | — | 待机档，低于第一档时用它，退出时也写回它。不得高于本分区第一档。 |
| `fallback_duty_cycle` | 是 | — | 兜底档：分区什么都读不到时用。不得低于该分区的 `default_duty_cycle`。 |
| `debounce_celsius` | 否 | `2.0` | 降档防抖（℃）。必须小于到下一档的间隔。 |
| `aggregation` | 否 | `max` | 分区内多源聚合：`max` 或 `mean`。 |
| `fallback_after_cycles` | 否 | `2` | 连续多少个周期没有读数才降到 `fallback_duty_cycle`，≥ 1。 |
| `command_timeout_seconds` | 否 | `10` | 单次 `ipmitool` 调用的超时，≥ 1。 |

整个文件里能省略并取默认值的只有五个：`debounce_celsius`、`aggregation`、
`fallback_after_cycles`、`command_timeout_seconds`，以及 `platform.toml` 里的
`ipmitool_path`。其余要么必填，要么是"从 `[global]` 继承"。

## `[[zones]]`

一个分区回答一个问题 —— 这几个引脚该多快 —— 输入可以是任意多个传感器。

| 字段 | 必填 | 默认值 | 说明 |
|---|---|---|---|
| `name` | 是 | — | 忽略大小写唯一。只用于日志，但日志行里显示的就是它。 |
| `monitor_sources` | 是 | — | 非空。每项是 `[sensors]` 的一个键，或 `GPU` / `GPU<n>`。 |
| `target_pins` | 是 | — | 非空。每个都必须在 `[pins]` 里声明。 |
| `thresholds` | 是 | — | 至少 1 条，按 `temperature` 严格升序。 |
| `aggregation` | 否 | 继承 `[global]` | `max` 或 `mean`，只统计本周期有读数的源。 |
| `default_duty_cycle` | 否 | 继承 `[global]` | 1–100，且不高于第一档。 |
| `fallback_duty_cycle` | 否 | 继承 `[global]` | 1–100，且不低于本分区的 `default_duty_cycle`。 |
| `fallback_after_cycles` | 否 | 继承 `[global]` | ≥ 1。 |
| `debounce_celsius` | 否 | 继承 `[global]` | ≥ 0，且小于到下一档的间隔。 |

### `[[zones.thresholds]]`

| 字段 | 必填 | 说明 |
|---|---|---|
| `temperature` | 是 | ℃，严格大于上一档。 |
| `duty_cycle` | 是 | 1–100。不要求递增，但一条会下降的曲线几乎一定是写错了。 |

每一档从它的温度生效，直到下一档；超过最后一档时保持最后一档的占空比。低于第一档时
分区停在 `default_duty_cycle`。

### 继承

五个字段是共享策略：`aggregation`、`default_duty_cycle`、`debounce_celsius`、
`fallback_duty_cycle`、`fallback_after_cycles`。写在 `[global]` 里是所有分区的默认
值，写在某个 `[[zones]]` 块里就是那个分区的覆盖。

继承在加载期一次性展开。控制循环里不存在"这个没写就去看全局值"的分支。

水泵是值得写清楚的那个例子：

```toml
[global]
default_duty_cycle = 30
fallback_duty_cycle = 60

[[zones]]
name = "pump"
monitor_sources = ["Water_Inlet"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100    # 从不慢转
fallback_duty_cycle = 100   # 也从不停止，即使读不到

[[zones.thresholds]]
temperature = 20
duty_cycle = 100
```

### 显卡源

`monitor_sources` 接受三种条目：

| 条目 | 读什么 |
|---|---|
| `"CPU_Tctl"`，任意 `[sensors]` 键 | 那个 BMC 传感器 |
| `"GPU"` | NVML 能看到的所有 N 卡中最高的那张 |
| `"GPU0"`、`"GPU1"`…… | 指定的一张卡（NVML device index） |

`GPU` 是保留名，`[sensors]` 不能定义它。跟 `GPU` 的分区跟的是最热的卡，跟 `GPU1`
的分区只看那一张 —— 这正是"两张卡共用一个冷排"与"另一张卡自带风扇"的区别。编号是
驱动的枚举顺序，不换卡、不重装驱动就不会变；`fanctl check` 会打印每张卡的编号、名字
和温度，让这个映射是"看得见"的而不是"假定"的。

`mean` 只对本周有读数的源求平均，所以一张缺失的卡不会把一个分区拖向 0 ℃。

## `[[follows]]`

引脚之间的约束，在所有分区都算完之后施加：

```toml
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

读作"前面板不得比冷排慢"。两端都必须被某个分区驱动；`pin` 不能跟它自己；成环的规则
在加载期被拒。多条规则会被求到不动点，所以结果与书写顺序无关。见
[设计](design-cn.md#合并)。

## 名字匹配

名字到处都按大小写不敏感比较：`cpu_fan`、`CPU_FAN`、`Cpu_Fan` 是同一个引脚，
`"gpu"` 就是 `"GPU"`，`"gpu1"` 就是 `"GPU1"`。

加载期会把 `config.toml` 里的每个引用改写成 `platform.toml` 里声明的大小写，然后才
精确比较 —— 所以 `check` 和 `run` 不可能对"指的是哪个引脚"产生分歧。`platform.toml`
里只差大小写的两个键会被拒。SDR 名字额外忽略分隔符：`CPU_Tctl_Value`、
`CPU Tctl Value`、`cpu-tctl-value` 是同一个传感器。

## 校验

`fanctl check` 和 `fanctl run` 启动时跑同一个校验器，一次列出全部问题：

- TOML 语法，以及未知字段；
- 阈值严格升序；`debounce_celsius` 小于相邻阈值间隔；
- 引用的引脚和传感器都存在；`[sensors]` 不占用 `GPU` 名字；
- `min_duty_cycle` ≤ `max_duty_cycle` ≤ 100，且 `min_duty_cycle` ≥ 1；
- 每个分区的 `default_duty_cycle` 在 `[min, max]` 内且不高于它自己的第一档；
- 每个分区的 `fallback_duty_cycle` 在 `[min, max]` 内，**且不低于它自己的
  `default_duty_cycle`** —— 读不到读数绝不能比"测到很凉"更安静；
- `fallback_after_cycles` ≥ 1、`command_timeout_seconds` ≥ 1、`interval_seconds` ≥ 1；
- `follows`：两端都被驱动、无自引用、无环；
- 忽略大小写后重名的键。

`GPU<n>` 是否存在是运行期事实，不是配置事实：`check` 会打印它枚举到的显卡，而引用
一张不存在的卡的分区会走"无读数"路径（先保持，再兜底），而不是启动失败。

## 一份真实配置

`examples/config.toml` 是一份完整的、带注释的起点。下面这套是 fanctl 当初为之编写的
那台机器上正在跑的策略 —— 水冷 EPYC，两张 4080 共用一个冷排，另有一张风冷 3080 ——
值得当作"各部件怎么配合"的例子来读,而不是当成对你那台机器的建议。

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

# 水泵：永远满转 —— 包括什么都读不到的时候，也包括退出路径
# （那里它被写回 100 %，而不是待机档）。
[[zones]]
name = "pump"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_4"]
default_duty_cycle = 100
fallback_duty_cycle = 100
[[zones.thresholds]]
temperature = 0
duty_cycle = 100

# CPU 散热器 + 后部排风，跟封装温度。
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

# 两张 4080 上方的冷排。3080 刻意不在里面：它有自己的风扇，把冷排绑到
# 所有显卡上，会让它在散不掉的热量上白转。曲线停在 70 %，那是这个机箱的
# 正压上限，不是冷排的热上限。
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

# 前面板：机箱里所有热源，包括自扇的 3080。顶部冷排吸的是机箱内的空气，
# 所以前面板是整台机器的进风口；只盯两张 4080，会让 3080 反复吸自己的排气。
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

# 正压：前面板不得比冷排慢。
[[follows]]
pin = "SYS_FAN_1"
min_of = "SYS_FAN_2"
```

上面每个占空比都是 5 % 的整数倍，因为 BMC 本来就把 PWM 量化成 5 %：这块板上 25、28、
30 % 都给出 828 RPM。写 27 只是在文件里写了个谎。每一档也在施加之后回读过转速 ——
曲线是计划，转速计才是证据。
