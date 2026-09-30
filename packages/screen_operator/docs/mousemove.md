# 鼠标移动：ydotool 绝对移动（`mousemove -a`）在本机 KWin 下失效

## 坑

本机（KWin / Plasma 6，分数缩放 ≈1.60）下，`ydotool mousemove -a -x X -y Y`
（绝对移动模式）**完全失效**：无论传什么坐标，虚拟光标都被推到 `(1,1)` 死区，
KWin 的 `workspace.cursorPos` 读到的坐标不随之变化。

验证过程（用 KWin 脚本读 `cursorPos`）：

```bash
# 真实鼠标移动时 cursorPos 是准的（手动放右下角 → 读到 1799,0）
# 但 ydotool 绝对移动后，所有输入都读到 (1,1)：
ydotool mousemove -a -x 0    -y 0    # → KWin 读到 (1, 1)
ydotool mousemove -a -x 1440 -y 900  # → KWin 读到 (1, 1)
ydotool mousemove -a -x 2880 -y 1800 # → KWin 读到 (1, 1)
```

而**相对移动（`mousemove` 不带 `-a`）是可靠的**，单位与 KWin `cursorPos` 同为
逻辑像素：

```bash
# 当前光标 (1573,1060)，相对 +100,+100（不越界时）精确 → (1673,1160)
ydotool mousemove -x 100 -y 100   # → KWin 读到 (1673,1160)
```

> 注意：相对移动若单步越界会被 KWin clamp 到屏幕边缘（如 y 超 1125 逻辑高会被
> 夹到 1124），大距离移动应分多步。

### 影响范围

- **原地点击（`click` 作用于当前光标）正常**——它不需要移动，只发 `click` 事件。
  这就是为什么"原地点击 text.md 有效"却"定位点击 Reload 无效"：定位点击 = 移动 + 点击，
  移动那步失效，点永远落在移动前的真实光标处。
- **绝对移动失效导致"自动定位点击"整条链断掉**，且失败是静默的（ydotool 返回 0、
  看似成功，光标却没动）。

### 坐标系

- ydotool 相对移动单位 = **逻辑像素**（与 KWin `cursorPos` 一致）。
- 本机屏幕物理 2880×1800，逻辑 1800×1125，scale = 系统 160% = 1.6。
- 业务侧拿到的"物理绝对坐标"（如 ScreenCast 窗口流 493×378、KWin 几何 × scale）
  要换算成逻辑再相对移动：`逻辑 = 物理 / scale`。

## 本 crate 的处理

`screen_operator` 把「注入」与「读数」拆成两个正交 trait，再用一个**后端无关**的
泛型组合层 `ScreenOperator<I: InputBackend, P: Probe>` 把「想到达某逻辑坐标」变成
「读 → 差 → 移 → 确认」的反复逼近：

- [`InputBackend`]：发「相对移一步 / 原地按一下」的原语。桌面实现 = `YdotoolBackend`
  （`mousemove -- DX DY` + `click`），移动端将来可另写 `AdbInjector`。
- [`Probe`]：读「当前指针在哪」。桌面实现 = `KwinProbe`（包 `KdeForegrounder::cursor_pos`）。
- [`operator`]：组合层，持有 `InputBackend`+`Probe`，内含闭环。桌面 =
  `ScreenOperator<YdotoolBackend, KwinProbe>`、移动端 = `ScreenOperator<AdbBackend, ScreenshotProbe>`，
  **共享同一套闭环**，分别实现接口、不共享注入代码。

`ScreenOperator<I, P>` 直接持有 `backend: I` + `probe: P`，内含闭环，
对外只暴露 `ensure_move_to(IVec2)` / `click_left_at(IVec2)` 这类直觉 API。所有入口
统一收 **KWin 逻辑坐标**（`glam::IVec2`），物理↔逻辑换算在「看→操作」边界做。

```rust
use screen_operator::{ScreenOperator, IVec2};
use ocr_agent::KdeForegrounder;

// 用 KdeForegrounder 组装桌面闭环：注入=ydotool，读数=fg.cursor_pos（逻辑坐标）。
let fg = KdeForegrounder::new("testing_08");
let op = ScreenOperator::new().with_foregrounder(fg);

// 入口收逻辑坐标（IVec2）；内部闭环：读当前 → 算差 → 发相对一步 → 等落盘，直至
// 偏差 ≤ 容差（不预设任何倍率，ydotool 相对移动落点不稳定，只能每步确认）。
op.ensure_move_to(IVec2::new(691, 562)).unwrap();
op.click_left_at(IVec2::new(691, 562)).unwrap();  // 闭环移动 + 原地点击
```

`ensure_move_to` / `click_at` 走相对移动闭环，绕开本机失效的 ydotool 绝对移动
（`mousemove -a`）。`click_left_at(pos)` = `click_at(pos, Left)` = 闭环移动 + 左键点击；
非左键用 `click_at(pos, btn)`。

### 读数的脏读过滤（KDE）

`KdeForegrounder::cursor_pos()` 已实现脏读过滤（连续读两次、差距 ≤ 容差才采信），
规避紧循环里 journalctl 偶发的陈旧行 race。它直接被 `KwinProbe` 复用，无需调用方
再包闭包。`cursor_pos` 返回的就是逻辑坐标，与 `ensure_move_to` 入口语义一致。

## 诊断清单（移动不生效时）

1. `ydotoold` 在跑？`systemctl --user status ydotool.service`。
2. 是绝对移动失效还是整条链失效？先测**原地点击**（`--click-current` / `click`）
   是否生效——生效说明 ydotool 注入 OK，问题在移动。
3. 移动不收敛 → 确认 `ScreenOperator` 已用 `with_foregrounder(fg)` 组装，且 `fg` 的
   `cursor_pos` 能返回有效坐标（KWin `cursorPos` 对真实鼠标准、对 ydotool 绝对移动
   不准，但对相对移动后准）。
4. 相对移动后仍偏 → 检查物理↔逻辑换算：目标应是逻辑坐标（KWin 逻辑宽 1800、高 1125，
   本机 scale = 系统 160% = 1.6）；若上游给的是物理像素需 ÷ scale。

## 相对移动单次可靠区实测（`move_rel` 原语）

`ScreenOperator::ensure_move_to` 闭环每步调一次 `InputBackend::move_rel`（即 ydotool `mousemove -- DX DY`）。
为确定"单次相对移动到底多可靠"，用 `examples/step_stability.rs` 做了**不同距离 × 多次**
扫描：每轮先 `ensure_move_to(起点)` 闭环回起点 → 读 `before = cursor_pos()`（KWin 真实坐标）
→ `move_rel((dist, 0))` → 读 `after = cursor_pos()` → **实际位移 = `after - before`（测量值）**
→ 误差 = 实际位移 − 指令距离（派生值）。

> 注意：**实际位移是测量出来的（`before`/`after` 都是 KWin 读数相减），误差才是算出来的。**
> 不要反过来从误差推实际位移。

### 坐标系前提

- 本机屏幕**逻辑**尺寸 1800×1125，坐标从 **0 开始**，故右边缘最大 x = **1799**、下边缘最大 y = 1124。
- 相对移动单步若越过屏幕边界，会被 **KWin 按屏幕几何 clamp** 到边缘（如 x 超 1799 夹到 1799）。

### 实测结论

| 单次指令距离 | 实测行为 |
|------|------|
| **≤ 400** | 精准（误差 0；400 八轮全 0 证实） |
| 410 | 轻微过冲（+5，轮 0 还准、轮 1 起稳定 +5~6） |
| 425 | 过冲 +13 |
| 450 | 过冲 +31 |
| 500 | 过冲 +66（实际移到 566） |
| 600~900 | 非线性暴涨，实际移量逼近屏幕右边缘 |
| **≥ 950** | 实际位移**饱和在 x≈1799**（屏幕右边缘），多余增量被 KWin clamp 丢弃 |

即：**ydotool `mousemove` 单次相对增量安全上限 ≈ 400 逻辑像素**；超过即进入过冲区，
≥950 直接被 clamp 到屏幕边缘 1799。不存在"绕回"——是单向饱和/clamp。

### 缓解：`ScreenOperator` 单步上限 `step_cap`

因单次大距离不可靠，`ScreenOperator::ensure_move_to` **不会一次性发整段 `delta`**，而是把每步增量各轴
`clamp(-step_cap, step_cap)`，`step_cap` 默认 **200**（远在 400 安全线以下，留 2× 余量），
大距离自然拆成多步逐个 `move_rel` + 读回逼近。可用 builder 链式覆盖：

```rust
use screen_operator::{ScreenOperator, YdotoolBackend, KwinProbe, KdeForegrounder};
let op = ScreenOperator::with_bin("ydotool").with_foregrounder(KdeForegrounder::new("app"))
    .with_step_cap(150); // 可选：调小更保守，调大更快（勿超 ~400）
```

验证：原 `move_rel(1200)` 会饱和到 1799（偏 599），但 `ensure_move_to((1200,562))` 经拆步后
**偏差 (0,0)**；移到右边缘 `(1799,562)` 同样 **偏差 (0,0)**，每步严格 +200。

### 闭环步数 / 容差对比实测（`move_probe` 实测）

`step_cap` 是「速度 vs 精度」旋钮：调大省步但单步过冲、靠闭环擦屁股；调小更稳。
`tolerance` 是「到达判定阈值」：调小（如 0）要求严格归零（多耗几步纠抖动），
调大（如 2，默认）允许 ≤2 偏差即停（更快、残差 ±1~2 在像素级可忽略）。

**对比 A：左下角 (0,1124) → 中心 (900,562)，`tolerance=2`，不同 `step_cap`**

| step_cap | 步数 | 过冲情况 | 最终偏差 |
|----------|------|---------|---------|
| 200（默认） | 5 | 无（每步精准） | (0,0) |
| 300 | 4 | 轻微（iter2 到 650 应 650） | (0,0) |
| 400 | 3 | iter0 到 412(+12)、iter2 到 920(+20) | (-1, 2) |
| 500 | 4 | iter0 到 537(+37)、iter2 到 1004(+4) | (-1, 0) |
| 1000 | 3 | 发 (900,-562) 实际到 (1153,405)（x+251、y+157 大过冲）→ 回退振荡 | (2, -1) |

→ 默认 200 零过冲零残差；300 是「提速且仍精准」的甜点（省 1 步、双向仍 (0,0)）；
≥400 过冲明显、留 ±1~2 残差，对角移动单步过冲可达 +251，**不建议**。

**对比 B：同距离 (0,1124)→(900,562)，`tolerance=0`（必须严格归零）vs `tolerance=2`**

| step_cap | tol=2 步数 | tol=0 步数 | tol=0 最终偏差 |
|----------|-----------|-----------|---------------|
| 200 | 5 | 5 | (0,0) |
| 300 | 4 | 5 | (0,0) |
| 400 | 3 | 5 | (0,0) |
| 1000 | 3 | 5 | (0,0) |

→ `tolerance=0` 把所有配置都拉回完美 (0,0)（消除 tol=2 下 ≥400 的 ±1~2 残差），
但代价是大 `step_cap` 的过冲被逼着回退、**省下的步数又耗在纠偏上**（步数回到 5，
与 cap=200 相同）。结论：**tol=0 下大 cap 既没省步又引入过冲振荡，纯劣势**；
只有 cap=200 + tol=0 是「零过冲且严格归零」最干净组合。

实战建议：默认 `step_cap=200` + `tolerance=2` 够用；若业务要求严格归零（如点按钮
必须精准），用 `tolerance=0` 但**保持 cap=200**。

### 原始数据

`examples/step_stability.rs --format json --out <文件>` 产出每轮原始记录
（`dist` / `round` / `before` / `after` / `err`），已留存于本目录：

- `step_stability_960_1000.json`：960/970/980/990/1000（边界饱和区）
- `step_stability_1000_1100.json`：1000/1100（绕回区验证，结论为单向饱和非绕回）
- `step_stability_375_450.json`：375/400/425/450（安全边界定位，**400 零误差**）
- `step_stability_410_420.json`：410/420（精确边界：400 准、410 起偏）

复现：`cargo run -p screen_operator --example step_stability -- --dists 1,2,3,... --format json --out docs/step_stability.csv`

## 指针加速度（`accel_profile`）实测记录

> 2026-08-01 实测：纠正了此前"KWin 下读不到 / 不管 ydotool 虚拟指针加速度"的错误推断。
> 进一步探查结论：**本环境（KDE + Xwayland）下无法把 ydotool 虚拟指针设为 flat**，加速度
> 实际处于 `adaptive` 并作用于相对移动落点。

### 能读，且 ydotool 虚拟指针就在 libinput 里

`libinput list-devices` 明确枚举了 ydotool 创建的虚拟设备，并带完整加速度档位：

```
Device:                  ydotoold virtual device
Kernel:                  /dev/input/event16
Id:                      virtual:2333:6666
Capabilities:            keyboard pointer
Accel profiles:          flat *adaptive custom   ← * 在 adaptive 上 = 当前生效档
```

- **能获取**：ydotool 虚拟指针（`/dev/input/event16`，`virtual:2333:6666`）的加速度档位直接可读，
  当前为 `adaptive`（默认）。KWin D-Bus / `kwinrc` 里**查不到**加速度接口（已确认：
  `org.kde.KWin` 无 mouse/input/accel 节点，`kwinrc`/`kdeglobals` 无 `AccelProfile`），
  但 libinput 层面能读到——**之前"KWin 读不到"的断言是错的**。
- **它受加速度管理**：有 `flat *adaptive custom` 三档，与物理鼠标一致。

### 900→1041：adaptive 下的实测

闭环把光标移到 `(0, 562)`（偏差 0,0），再单步 `move_rel((900, 0))`，读前后 `cursor_pos`：

```
[move_probe] 结束: KWin读逻辑=([0, 562]), 偏差=([0, 0])
[raw_step]   指令增量=([900, 0]), 实际偏移=([1041, 0]), 误差=(141, 0)
```

发 900 实际走 1041（多 141，约 +15.7%）。结合源码可定位来源（见下）。

### 源码实证：ydotool 不加工增量，关加速度在 Wayland 下失效

读 `/home/aa/repos/auto_ls/learn_ls/ydotool`（上游 `ReimuNotMoe/ydotool`）：

- `Client/tool_mousemove.c`：相对移动直接 `uinput_emit(EV_REL, REL_X, pos[0])`，
  **把 900 原样发出，不倍率、不钳制、不分段**。
- `Daemon/ydotoold.c` 主循环（`recv → write`）：**只转发，不改增量**。
- `Daemon/ydotoold.c` 启动分支（~L357-373）：

  ```c
  if (getenv("DISPLAY")) {                    // 本环境 DISPLAY=:0，进入
      if (stat("/usr/bin/xinput") == 0) {
          execl("xinput", "--set-prop", "pointer:ydotoold virtual device",
                "libinput Accel Profile Enabled", "0,", "1", NULL);  // 想设 flat
      } else { printf("xinput ... not disabling ..."); }
  }
  ```

  **本环境实测**：`DISPLAY=:0` 有、xinput 已装，但 xinput 连的是 **Xwayland**
  （`WARNING: running xinput against an Xwayland server`），碰不到 Wayland 原生的
  ydotool 虚拟指针 → 关加速度**无效**。重启 `ydotool.service` 后设备仍 `adaptive`，
  佐证该分支在本环境结构性失效（非"无 DISPLAY"、非"xinput 缺失"——两者都曾误判，
  真正原因是 Xwayland 下 xinput 管不到 Wayland 设备）。

### 试图用 libinput quirk 强制 flat —— 走不通

写 `/etc/libinput/99-ydotool-flat.quirks`（`MatchName=*ydotoold virtual device*` +
`AttrAccelProfile=flat`）并重启 ydotoold，结果：

```
quirks error: Unknown key AttrAccelProfile in [ydotool virtual device]
```

**libinput 1.31.3 的 quirk 系统根本没有加速度相关键**（`Attr*` 列表无 accel profile/speed，
系统 quirk 文件也无 accel）。加速度档位是**运行时**通过 xinput/桌面环境设的，
quirk 层管不了 → 该文件已删除（留着会让 libinput 每次解析报错）。

### 试图用 Rust + libinput C API 设 flat —— 能 set，但不作用于 KWin（per-context）

libinput 是 C 库，Rust 可用 `input` crate（libinput 绑定，0.10.0）直接调
`Device::config_accel_set_profile(AccelProfile::Flat)`。在 `tools/accel_set` 写了探测+设置
程序（`Libinput::new_with_udev` + `udev_assign_seat("seat0")`，枚举到
`ydotoold virtual device` 后 set）。实测：

```
设备: "ydotoold virtual device" | pointer=true | 当前档=Some(Adaptive) | 支持档=[Flat, Adaptive]
  ↑ ydotool 虚拟设备: set 前当前档=Some(Adaptive), 支持 Flat=true
    ✓ set_profile(Flat) 请求已发; 同上下文读回当前档=Some(Flat)
```

- **可见性/权限 OK**：以 `input` 组用户运行，libinput 能枚举到 ydotool 虚拟设备（KWin 未
  独占设备 fd），且它**支持 Flat 档**。
- **同上下文 set 成功**：set 后在我们自己的 libinput 上下文里 `config_accel_profile()` 读回
  变 `Flat`。

**但对外无效**（决定性实测）：set 后立刻从**另一个进程**经 ydotool 发 `move_rel((900,0))`、
用 KWin `cursor_pos` 读落点：

```
[move_probe] 结束: KWin读逻辑=([0, 562]), 偏差=([0, 0])
[raw_step]   指令增量=([900, 0]), 实际偏移=([1057, 0]), 误差=(157, 0)
```

实际偏移仍是 **1057**（与 adaptive 下的 1041 同一量级，非 900）——且外部
`libinput list-devices` 仍显示 `flat *adaptive`（`*` 在 adaptive）。

**根因**：libinput 的加速度 filter 是 **per-context** 的——每个消费者（KWin、我们的程序）
各自维护独立的 accel 状态。我们改的是**自己上下文**的 filter，而真正处理光标移动的是
**KWin 的 libinput 上下文**，它仍是 adaptive，不受我们 set 影响。ydotool 发的 `REL_X` 经
KWin 的 adaptive filter → 落点照旧被加速度扭曲。

→ 这条"Rust 直接调 libinput API"的路**也走不通**：能 set，但改不到 KWin 实际使用的上下文。

### 通过 KWin 设备级 D-Bus 设 flat —— ✅ 成功（正确通道）

读 KWin 源码（`src/backends/libinput/`）发现：KWin 在 Wayland 下用自带的 libinput 后端读
输入设备，对每个设备用 `Device` 封装并**自己维护 accel 状态**（`device.cpp:564`
`libinput_device_config_accel_set_profile`）。关键：`device.cpp:476` 把每个设备注册到 D-Bus：

```cpp
QDBusConnection::sessionBus().registerObject(
    QStringLiteral("/org/kde/KWin/InputDevice/") + m_sysName,
    device, QDBusConnection::ExportAllProperties);
```

即**每个输入设备在 `/org/kde/KWin/InputDevice/<sysName>` 导出所有 Q_PROPERTY**，其中包括
`pointerAccelerationProfileFlat`（**readwrite**）。这正作用在 **KWin 自己的 libinput 上下文**
里——是 per-context 难题的「正确一侧」，也是前面所有外部通道失败的根因（外部动的是错的 context）。

ydotool 虚拟设备对应 `/org/kde/KWin/InputDevice/event16`（sysName 即 event 号，可能随
ydotoold 重启变化，但设备名恒为 `ydotoold virtual device`）。实测：

```
# 设前
pointerAccelerationProfileFlat = false
# 经 D-Bus 设为 true
dbus-send --session --dest=org.kde.KWin --print-reply \
  /org/kde/KWin/InputDevice/event16 \
  org.freedesktop.DBus.Properties.Set \
  string:org.kde.KWin.InputDevice string:pointerAccelerationProfileFlat variant:boolean:true
# 设后读回
pointerAccelerationProfileFlat = true   ✓
# 外部 libinput 同步变为 flat *adaptive
```

**落点实测（决定性）**：设 flat 后，归零到 (0,562) 再单步 `move_rel((900,0))`：

```
[move_probe] 结束: KWin读逻辑=([0, 562]), 偏差=([0, 0])
[raw_step]   指令增量=([900, 0]), 实际偏移=([900, 0]), 误差=(0, 0)
```

实际偏移 **900**（误差 0），对比 adaptive 下的 1041 —— **加速度就是那 +141 的来源，且现已
彻底消除**。这同时反向实锤：之前 adaptive 下的过冲确实来自 KWin 上下文的加速度（而非 ydotool
大增量自身），因为 flat 后 900 精确归位、无任何过冲。

> **无需恢复**：`event16` 是 ydotool 的 **uinput 虚拟设备**，与真实鼠标是 libinput 里两个独立
> `Device`，设它的 `pointerAccelerationProfileFlat` **不影响真实鼠标手感**。让 ydotool 虚拟设备
> 常驻 flat 是更健康的状态（ydotool 官方也试图关它的加速度，只是 Xwayland 下 xinput 没成功）。
> 故只需「确保 flat」，不要「移动后恢复」。重启 ydotoold 会把设备重置回 adaptive，因此应采用
> **幂等确保**（移动前读一下，不是 flat 就设 true，已是则不动），而非一次性设置。

### 结论

- **KWin/Xwayland 下有关 flat 的通道，且是正确通道**：KWin 设备级 D-Bus 属性
  `/org/kde/KWin/InputDevice/<sysName>/pointerAccelerationProfileFlat`（readwrite）。前面误判
  「KWin 无接口」是因为查错了地方——accel 不在 `org.kde.KWin` 顶层方法，而在**每个输入设备的
  D-Bus 对象**上（由 KWin 源码 `device.cpp:476` 的 `ExportAllProperties` 证实）。
- **失败通道的根因统一为 per-context**：xinput（Xwayland 碰不到 Wayland 设备）、libinput quirk
  （无 accel 键）、外部 Rust libinput（独立 context，KWin 不共享）——三者都动不到 KWin 自己的
  context；只有 KWin D-Bus 动的是正确的 context。
- **加速度（adaptive）确实作用于 `REL_X`**，是 900→1041 的来源（已用 flat 对照实测 900→900 反证）。
- **对 screen_operator**：移动前通过 KWin D-Bus **幂等确保** ydotool 虚拟设备 flat（不恢复），
  单步「指令 = 实际位移」，闭环更高效且无加速度干扰。详见 `accel.rs`（`ensure_flat`）。


### 对 screen_operator 的影响与后续选项

- 关不掉加速度 → 移动前"读→关(flat)→移动→恢复"在当前环境**无法落地**（Sway 侧用
  `swaymsg` 可落地，KWin/Xwayland 不行）。**已放弃该 guard 的 KWin 实现**。
- 现有 `ScreenOperator` 闭环（读→差→移→确认，默认 `step_cap=200`）**每步读回确认，会自动吸收
  加速度带来的倍率偏差**（某步偏了，下一步差值补偿），所以最终仍能收敛到目标，只是
  每步实际位移 ≠ 指令增量、迭代次数比"flat 理想"略多。实测大距离仍能精确归零，故
  **加速度不破坏正确性，只影响每步效率**。
- 后续可选（均未做）：① 改 ydotoold 源码用 Wayland/libinput 运行时接口关加速度；
  ② 换 Sway 后端用 `swaymsg` 关；③ 接受 adaptive，靠闭环吸收（当前采用）。

## 参考

- ydotool `mousemove` 帮助明确提示：`You need to disable mouse speed acceleration
  for correct absolute movement.` —— 本机即使如此，绝对模式仍失效，故改相对模式。
- 绝对模式死区 `(1,1)` 是 uinput 虚拟设备 `ABS_X/ABS_Y` 轴范围未正确初始化所致，
  相对事件（`REL_X/REL_Y`）不受影响。
