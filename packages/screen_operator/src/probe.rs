//! 读数/确认抽象：告诉闭环「现在指针/手指在哪」。
//!
//! 与 [`InputBackend`]（发指令）正交——本 trait 只负责「读状态」，不负责「发指令」。
//! 桌面（KWin `cursorPos`）/ X11（xdotool query pointer）各自实现；移动端没有持久
//! 光标，可返回 `Ok(None)`，那时闭环改用截图+OCR 回看（那是另一套 `Probe` 实现）。
//!
//! 见 [`InputBackend`] 文档：为何注入与读数拆成两个独立 trait。

use anyhow::Result;
use glam::IVec2;

/// 读数原语：返回当前指针/手指的**逻辑坐标**；无光标（如移动端）返回 `Ok(None)`。
pub trait Probe {
    fn pointer_pos(&self) -> Result<Option<IVec2>>;
}

/// KWin（KDE/Wayland）读数：经 KWin D-Bus `Scripting` 跑 JS 读 `workspace.cursorPos`。
///
/// 实现直接复用 [`crate::foregrounder::KdeForegrounder::cursor_pos`]，那里已做脏读
/// 过滤（连续读两次、差距 ≤ 容差才采信），规避 journalctl 紧循环里的陈旧行 race。
#[derive(Clone)]
pub struct KwinProbe {
    fg: crate::foregrounder::KdeForegrounder,
}

impl KwinProbe {
    pub fn new(fg: crate::foregrounder::KdeForegrounder) -> Self {
        Self { fg }
    }
}

impl Probe for KwinProbe {
    fn pointer_pos(&self) -> Result<Option<IVec2>> {
        // cursor_pos 返回 Result<IVec2>，这里包成 Option：读不到坐标（非致命）即 None，
        // 让 ScreenOperator 退化为不依赖光标的路径（当前桌面不会触发，移动端会）。
        Ok(self.fg.cursor_pos().ok())
    }
}
