//! 屏幕操作器核心：在屏幕绝对坐标上注入鼠标 / 键盘输入。
//!
//! 分层（详见 [`injector`] / [`probe`]）：
//! - [`input_backend::InputBackend`]：发「相对移一步 / 当前点一下 / 键入」的原语（ydotool 等）
//! - [`probe::Probe`]：读「当前指针在哪」（KWin 等）
//! - 本 `ScreenOperator<I: InputBackend, P: Probe>` 是**组合层**：持有输入后端 + 读数器，
//!   内含「读→差→移→确认」闭环（[`ensure_move_to`]），对外暴露直觉 API
//!   （`ensure_move_to` / `click_left_at` / `key` …）。调用方无需感知底层是 ydotool 还是别的。
//!
//! 为什么 `ScreenOperator` 直接泛型、没有独立的 `Mover` 闭环类型：闭环（ensure_move_to）
//! 只在 `InputBackend` 之上多了一件事——反复「读 Probe → 算差值 → 发 move_rel → 确认」。
//! 它本质就是组合层自己的职责，抽成独立泛型骨架只会变成「指针原语转发器」的冗余层
//! （且造成指针一等、键盘二等的不对称）。让 `ScreenOperator` 直接持有 `backend`+`probe`
//! 并内含闭环，三层（InputBackend 发 / Probe 读 / ScreenOperator 组合）各司其职，最干净。

use std::time::Duration;

use anyhow::Result;
use glam::IVec2;
use tracing::warn;

use crate::input_backend::{InputBackend, YdotoolBackend};
use crate::keycode::keycode_of;
use crate::mouse::MouseButton;
use crate::probe::{KwinProbe, Probe};

/// 屏幕操作器（组合层）：持有输入后端 + 读数器，内含移动闭环。
///
/// 泛型 `I: InputBackend`（怎么注入）+ `P: Probe`（怎么读数），编译期确定后端。
/// 桌面常用组合 = `ScreenOperator<YdotoolBackend, KwinProbe>`，由 [`ScreenOperator::new`]
/// 等桌面专用构造器直接产出，调用方通常不必写出类型参数。
///
/// 坐标语义统一为 **KWin 逻辑坐标**（与 `cursor_pos` / `screen_logical_size` 同套，本机
/// 1800×1125）；物理↔逻辑换算在「看→操作」边界做，不塞进本层。
#[derive(Clone)]
pub struct ScreenOperator<I: InputBackend, P: Probe> {
    /// 输入后端（指针 + 键盘原子原语）。
    backend: I,
    /// 读数器（读当前指针逻辑坐标）。
    probe: P,
    /// 单步相对移动上限（逻辑像素/轴）。`ensure_move_to` 把大距离拆成 ≤ 此值的子步逐个
    /// `backend.move_rel` + 读回，避免单次 `move_rel` 触发 ydotool 的不可靠区（实测单次
    /// 增量 ≥~425 开始过冲、≥~950 被 KWin clamp 到屏幕边缘 1799）。默认 200，远在
    /// 400 实测安全线以下，留足余量。
    step_cap: i32,
    /// 到达容差（逻辑像素/轴）。`ensure_move_to` 判定「已到达」的偏差阈值：当前与目标各轴
    /// 差的绝对值均 ≤ `tolerance` 即停止。默认 2（读 `cursor_pos` 有 ±1 脏读抖动，2 足够
    /// 吸收且不会过早停在大偏差处）。
    tolerance: i32,
}

impl<I: InputBackend, P: Probe> std::fmt::Debug for ScreenOperator<I, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenOperator").finish()
    }
}

/// 桌面专用构造器（固化为 `YdotoolBackend` + `KwinProbe`）。
///
/// 其余后端无关方法在 [`ScreenOperator`] 的主 impl 块；这里只放依赖具体桌面后端的入口。
impl ScreenOperator<YdotoolBackend, KwinProbe> {
    /// 用默认的 `ydotool` 后端构造。要求系统已安装 `ydotool` 且 `ydotoold` 在运行
    /// （`systemctl --user enable --now ydotool.service`）。
    ///
    /// 注意：默认构造不含有效的 KWin 读数（target 为空），移动/点击闭环需在调用前
    /// 用 [`with_foregrounder`] 组装目标窗口，否则 `Probe` 读不到光标。
    pub fn new() -> Self {
        Self {
            backend: YdotoolBackend::new("ydotool"),
            probe: KwinProbe::new(crate::foregrounder::KdeForegrounder::new("")),
            step_cap: 200,
            tolerance: 2,
        }
    }

    /// 用自定义后端可执行文件构造（如指定 `ydotool` 的绝对路径，或将来换 uinput 封装）。
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self {
            backend: YdotoolBackend::new(bin),
            probe: KwinProbe::new(crate::foregrounder::KdeForegrounder::new("")),
            step_cap: 200,
            tolerance: 2,
        }
    }

    /// 用 [`KdeForegrounder`] 组装桌面闭环：注入用 ydotool、读数用该 fg 的 `cursor_pos`。
    /// `cursor_pos` 返回的就是逻辑坐标，与本层入口语义一致。这是 KDE/Wayland 下最常用
    /// 的入口；非 KDE 平台可手动构造各自的 `InputBackend`+`Probe` 再 `ScreenOperator::new`。
    pub fn with_foregrounder(mut self, fg: crate::foregrounder::KdeForegrounder) -> Self {
        self.probe = KwinProbe::new(fg);
        self
    }
}

impl<I: InputBackend, P: Probe> ScreenOperator<I, P> {
    /// 用任意后端组合构造（移动端 / X11 等非桌面后端走这里）。
    pub fn with_backend_probe(backend: I, probe: P) -> Self {
        Self {
            backend,
            probe,
            step_cap: 200,
            tolerance: 2,
        }
    }

    /// 链式覆盖底层闭环的单步上限（默认 200 逻辑像素/轴）。调大可加快大距离移动
    /// （但别超过 ~400 实测安全线），调小更保守。仅影响 `ensure_move_to` 内部的拆步粒度。
    pub fn with_step_cap(mut self, cap: i32) -> Self {
        self.step_cap = cap;
        self
    }

    /// 链式覆盖底层闭环的到达容差（默认 2 逻辑像素/轴）。调小要求更精准才停（可能多耗
    /// 几步纠脏读抖动），调大更早停（更快但落点可能偏几像素）。
    pub fn with_tolerance(mut self, tolerance: i32) -> Self {
        self.tolerance = tolerance;
        self
    }

    // ---- 鼠标：移动 ----

    /// 把鼠标指针移动到屏幕绝对坐标 (x, y)（**绝对模式 `-a`**）。
    ///
    /// 仅作为绝对模式回退 API 暴露给外部；本机 KWin/Wayland 下绝对模式通常失效，
    /// 移动请优先用 [`ensure_move_to`]（相对闭环）。透传 [`InputBackend::move_abs`]。
    pub fn move_abs(&self, pos: IVec2) -> Result<()> {
        self.backend.move_abs(pos)
    }

    /// **确保**移动鼠标到逻辑坐标 `pos`（不点击）。
    ///
    /// 反复「读当前 → 算差值 → 发相对一步 → 等落盘」直至偏差 ≤ `tolerance` 或达 `MAX_ITER`。
    /// 靠每步确认收敛，不预设任何倍率常数——ydotool 相对移动落点不稳定（单步量不固定、
    /// 大指令过冲甚至撞墙），单次移动可能落不到，故名为 `ensure_move_to`（确保**到达**），
    /// 区别于 [`move_abs`]（发一次绝对命令就返回）。
    ///
    /// **坐标语义**：`pos` 为 **KWin 逻辑坐标**（与 `cursor_pos` 返回值同套，本机
    /// 1800×1125）。ydotool 虚拟设备加速度的 flat 确保已收口到 `YdotoolBackend::new`
    /// （构造时一次性），不在此处掺入具体后端细节。详见 accel.rs / injector.rs。
    pub fn ensure_move_to(&self, pos: IVec2) -> Result<()> {
        const MAX_ITER: usize = 80;
        const READ_RETRY: usize = 3; // 单次读光标失败时的重试次数。
        const SETTLE: Duration = Duration::from_millis(110); // 等光标真正移动 + 日志落盘。
        let tolerance = self.tolerance; // 到达容差（可链式配置）。

        for iter in 0..MAX_ITER {
            // 读当前位置：读不到时在本轮内重试；一直失败才放弃（不静默停在原地）。
            let mut cur: Option<IVec2> = None;
            for _ in 0..READ_RETRY {
                if let Some(p) = self.probe.pointer_pos()? {
                    cur = Some(p);
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            let curr = match cur {
                Some(p) => p,
                None => {
                    warn!(target: "screen_operator::move", iter, "ensure_move_to 读位置连续失败，放弃本轮");
                    return Ok(());
                }
            };

            let delta = pos - curr;
            if delta.x.abs() <= tolerance && delta.y.abs() <= tolerance {
                tracing::debug!(target: "screen_operator::move", iter, cur = %format!("({curr})"), target = %format!("({pos})"), "ensure_move_to 已到达");
                return Ok(());
            }
            // 拆步：单步增量各轴夹到 [-step_cap, step_cap]，避免单次 move_rel 触发
            // ydotool 不可靠区（≥~425 过冲、≥~950 被 clamp 到屏幕边缘）。下一轮循环
            // 会重新读位置并继续逼近剩余差值，自然把大距离拆成多步。
            let step = IVec2::new(
                delta.x.clamp(-self.step_cap, self.step_cap),
                delta.y.clamp(-self.step_cap, self.step_cap),
            );
            tracing::debug!(target: "screen_operator::move", iter, cur = %format!("({curr})"), target = %format!("({pos})"), cmd = %format!("({step})"), "ensure_move_to 发单步");
            self.backend.move_rel(step)?;
            std::thread::sleep(SETTLE);
        }
        Ok(())
    }

    /// 原语：相对当前位置移一步 `delta`（逻辑像素），**不读回确认、不闭环**。
    ///
    /// 透传 [`InputBackend::move_rel`]。供外部直接调用「移动一次」原语——例如验证 ydotool
    /// 注入本身、或实现自定义逼近。绝大多数业务应走 [`ensure_move_to`]（闭环、每步确认落点），
    /// 不要单独用本方法，单次不保证落点准。
    pub fn move_rel(&self, delta: IVec2) -> Result<()> {
        self.backend.move_rel(delta)
    }

    /// 在逻辑坐标 `pos` 处用 `btn` 键单击一次（按下 + 抬起）。先闭环移动到 `pos`，再原地点击。
    ///
    /// **坐标语义**：`pos` 为逻辑坐标（与 `cursor_pos` 同套）。
    pub fn click_at(&self, pos: IVec2, btn: MouseButton) -> Result<()> {
        self.ensure_move_to(pos)?;
        self.backend.click(btn)
    }

    /// 左键单击（最常用，便捷封装）。
    pub fn click_left_at(&self, pos: IVec2) -> Result<()> {
        self.click_at(pos, MouseButton::Left)
    }

    /// 在屏幕绝对逻辑坐标 `pos` 处双击（左键两次）。先闭环移动到 `pos`，再当前位置双击。
    /// 与 [`click_at`] 对称（移动到 `pos` 再单击）。
    pub fn double_click_at(&self, pos: IVec2, btn: MouseButton) -> Result<()> {
        self.ensure_move_to(pos)?;
        self.double_click(btn)
    }

    /// 在**当前鼠标位置**单击（不移动鼠标）。用于验证执行器注入本身是否生效
    /// （如用户在编辑器里把光标放到某处，原地点一下看是否有响应）。透传
    /// [`InputBackend::click`]。
    pub fn click(&self, btn: MouseButton) -> Result<()> {
        self.backend.click(btn)
    }

    /// 在**当前鼠标位置**双击（不移动鼠标）。透传 [`InputBackend::double_click`]。
    pub fn double_click(&self, btn: MouseButton) -> Result<()> {
        self.backend.double_click(btn)
    }

    // ---- 鼠标：按下 / 抬起（用于拖拽）----

    /// 在屏幕绝对逻辑坐标 `pos` 处**按下** `btn` 不抬起（配合 [`release_at`] 实现拖拽）。
    /// 先闭环移动到 `pos`，再当前位置按下。
    pub fn press_at(&self, pos: IVec2, btn: MouseButton) -> Result<()> {
        self.ensure_move_to(pos)?;
        self.backend.press(btn)
    }

    /// 在屏幕绝对逻辑坐标 `pos` 处**抬起** `btn`（与 [`press_at`] 配对）。
    /// 先闭环移动到 `pos`，再当前位置抬起。
    pub fn release_at(&self, pos: IVec2, btn: MouseButton) -> Result<()> {
        self.ensure_move_to(pos)?;
        self.backend.release(btn)
    }

    /// 拖拽：从 `from` 闭环移动到并按下 `btn` → 闭环移动到 `to` → 抬起。
    pub fn drag(&self, from: IVec2, to: IVec2, btn: MouseButton) -> Result<()> {
        self.press_at(from, btn)?;
        // press_at 已闭环移动到 from；这里再闭环移动到 to 后抬起。
        self.ensure_move_to(to)?;
        self.release_at(to, btn)
    }

    // ---- 键盘（透传 InputBackend 原语；名字→keycode 翻译留在本层）----

    /// 键入一段文本（相当于「粘贴式」输入，不经按键布局展开）。
    /// 适合填表、输命令等；若需模拟真实逐键，用 [`ScreenOperator::key`]。
    /// 透传 [`InputBackend::type_text`]。
    pub fn type_text(&self, text: &str) -> Result<()> {
        self.backend.type_text(text)
    }

    /// 按一次键（按下 + 抬起）。`name` 接受两种写法：
    /// - `KEY_*` 键名（大小写不敏感），如 `"KEY_ENTER"`、`"KEY_A"`、`"KEY_F5"`、
    ///   `"KEY_LEFTCTRL"`（见 [`keycode_of`]）；
    /// - 纯数字 keycode（十进制或 `0x` 十六进制），如 `"31"`、`"0x1F"`。
    ///
    /// 内部把名字翻译成 Linux keycode 数字，再透传 [`InputBackend::key`]（ydotool 的
    /// `key` 只认数字码，直接透传 `KEY_*` 名字会被当成 0 静默失效）。
    pub fn key(&self, name: &str) -> Result<()> {
        let code = keycode_of(name)?;
        self.backend.key(code)
    }

    /// 按下某键不抬起（键名写法同 [`key`]），配合 [`key_up`] 实现组合键
    /// （如 Shift+A = key_down("KEY_SHIFT") + key("KEY_A") + key_up("KEY_SHIFT")）。
    /// 更省事用 [`ScreenOperator::combo`]。
    pub fn key_down(&self, name: &str) -> Result<()> {
        let code = keycode_of(name)?;
        self.backend.key_down(code)
    }

    /// 抬起某键（与 [`key_down`] 配对）。键名写法同 [`key`]。
    pub fn key_up(&self, name: &str) -> Result<()> {
        let code = keycode_of(name)?;
        self.backend.key_up(code)
    }

    /// 直接按数字 keycode（不做名字翻译），按下 + 抬起。适合表外特殊键。
    /// 透传 [`InputBackend::key`]。
    pub fn key_code(&self, code: u16) -> Result<()> {
        self.backend.key(code)
    }

    /// 发送组合键，如 `combo(&["KEY_LEFTCTRL", "KEY_S"])` 即 Ctrl+S。
    ///
    /// 键名写法同 [`key`]（`KEY_*` 名或数字码），内部先翻译成数字 keycode，再把
    /// 「依次按下 + 逆序抬起」交给 [`InputBackend::combo`]（时序由后端自身保证）。等价于：
    /// `ydotool key 29:1 31:1 31:0 29:0`（Ctrl+S 的数字码）。
    pub fn combo(&self, keys: &[&str]) -> Result<()> {
        if keys.is_empty() {
            anyhow::bail!("combo 需要至少一个键名");
        }
        let codes: Vec<u16> = keys
            .iter()
            .map(|k| keycode_of(k))
            .collect::<Result<Vec<_>>>()?;
        self.backend.combo(&codes)
    }
}
