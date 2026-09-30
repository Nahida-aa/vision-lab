//! 输入后端抽象：把「一步输入操作」打给系统（指针 + 键盘）。
//!
//! 与 [`Probe`]（读状态）正交——本 trait 只负责「发指令」，不负责「读结果」。
//! 桌面（ydotool）/ 移动端（adb）/ X11（xdotool）各自实现本 trait，闭环骨架
//! [`ScreenOperator`](组合层) 只认 `InputBackend`，不感知具体后端。
//!
//! 为什么分 `InputBackend` 和 `Probe` 两个独立 trait（而非打包成一个 `ScreenInput`）：
//! 注入通道与读数来源在跨端时**必然解耦**——桌面恰巧 ydotool+KWin 配对，但移动端
//! 注入走 adb、确认走截图，两端没有共同的上层概念。打包成一个 trait 会逼移动端
//! 的 `cursor_pos` 永远返回 `None`、桌面被迫理解 touch，互相拖累。分别抽象更干净。
//!
//! 为什么指针**和**键盘都在同一个 `InputBackend`（而非拆成 `Pointer`/`Keyboard` 两个
//! trait）：输入注入是一个完整后端的能力，指针与键盘不可分割——移动端 adb 同样有
//! `input text` / `input keyevent`（键盘注入是真实存在的后端能力，不是空想）。拆开
//! 会逼每个后端实现两个 trait、组合层持有两份泛型，徒增复杂度而无收益。键盘和指针
//! 的区别由方法分组体现（见下方原语清单），而非靠 trait 边界割裂。

use anyhow::{Context, Result};
use glam::IVec2;

use crate::ensure_ydotool_flat;
use crate::mouse::MouseButton;

/// 输入后端原语：后端无关的「发一步输入操作」能力集合（指针 + 键盘）。
///
/// 全部是**原子原语**（一次命令即一动作），不含任何闭环/组合逻辑——组合（如
/// 拖拽、移动+点击、组合键）由 [`crate::operator::ScreenOperator`] 用
/// 这些原语拼。
///
/// 指针原语分两类：
/// - **相对**原语（`move_rel` / `click` / `press` / `release` / `double_click`）：作用于
///   「当前指针位置」，与具体坐标无关，跨端统一。
/// - **绝对**原语（`move_abs`）：直接指定目标逻辑坐标，本机 KWin/Wayland 下通常
///   失效，仅作回退 API 暴露；闭环移动走 [`crate::operator::ScreenOperator::ensure_move_to`]（相对+读回确认）。
///
/// 键盘原语（`type_text` / `key` / `key_down` / `key_up` / `combo`）接收**已翻译的 Linux
/// keycode 数字**（见 [`crate::keycode::keycode_of`]）——名字→数字的翻译留在组合层，
/// 后端只认数字码，避免各后端重复实现键名映射。
///
/// 为什么把这些原语都收进 trait 而非留在 `operator` 直接 spawn：这些是**注入原语**，
/// 本就该属于 `InputBackend` 的抽象边界。留在 `operator` 直接发 ydotool 会让「后端无关」
/// 的承诺落空——移动端 `AdbBackend` 将永远拿不到完整的输入语义，被迫在调用层重写。
pub trait InputBackend {
    // ---- 指针：相对（作用于当前位置）----

    /// 相对当前位置偏移 `delta`（逻辑像素）。桌面即 ydotool `mousemove -- DX DY`；
    /// 移动端可转成 touch down+move（或直接 swipe 增量）。
    fn move_rel(&self, delta: IVec2) -> Result<()>;

    /// 在当前位置单击一次 `btn`（按下+抬起）。桌面即 ydotool `click <code>`；移动端即
    /// down+up。带 `btn` 参数（而非硬编码左键），使非左键点击也是同一原语的不同参数。
    fn click(&self, btn: MouseButton) -> Result<()>;

    /// 在当前位置**按下** `btn` 不抬起（与 [`release`] 配对实现拖拽）。桌面即 ydotool
    /// `click <down_code>`；移动端即 touch down。
    ///
    /// [`release`]: InputBackend::release
    fn press(&self, btn: MouseButton) -> Result<()>;

    /// 在当前位置**抬起** `btn`（与 [`press`] 配对）。桌面即 ydotool `click <up_code>`；
    /// 移动端即 touch up。
    ///
    /// [`press`]: InputBackend::press
    fn release(&self, btn: MouseButton) -> Result<()>;

    /// 在当前位置双击 `btn`（按下+抬起两次）。桌面即 ydotool `click -D 60 <code>
    /// `<code>`；移动端即两次 down+up。属「当前位置点击」原语，与 [`click`] 同级。
    ///
    /// [`click`]: InputBackend::click
    fn double_click(&self, btn: MouseButton) -> Result<()>;

    // ---- 指针：绝对 ----

    /// 绝对定位到逻辑坐标 `pos`。桌面即 ydotool `mousemove -a -x X -y Y`；移动端可转成
    /// `input tap` 类绝对指令。本机 KWin/Wayland 下通常失效，闭环移动请用
    /// [`crate::operator::ScreenOperator::ensure_move_to`]（相对+读回确认）；本方法仅作
    /// 回退/外部直接调用原语保留。
    fn move_abs(&self, pos: IVec2) -> Result<()>;

    // ---- 键盘（接收已翻译的 Linux keycode 数字）----

    /// 键入一段文本（相当于「粘贴式」输入，不经按键布局展开）。适合填表、输命令等。
    /// 桌面即 ydotool `type <text>`；移动端即 `input text <text>`。
    fn type_text(&self, text: &str) -> Result<()>;

    /// 按一次键（按下 + 抬起）。`code` 为 Linux keycode 数字（见
    /// [`crate::keycode::keycode_of`]）。桌面即 ydotool `key CODE:1 CODE:0`。
    fn key(&self, code: u16) -> Result<()>;

    /// 按下某键不抬起（与 [`key_up`] 配对实现组合键）。
    ///
    /// [`key_up`]: InputBackend::key_up
    fn key_down(&self, code: u16) -> Result<()>;

    /// 抬起某键（与 [`key_down`] 配对）。
    ///
    /// [`key_down`]: InputBackend::key_down
    fn key_up(&self, code: u16) -> Result<()>;

    /// 发送组合键：依次按下 `codes` 各键、再逆序抬起（时序由后端自身保证）。
    /// 桌面即一条 ydotool `key` 命令；移动端可转成多次 `input keyevent`。
    fn combo(&self, codes: &[u16]) -> Result<()>;
}

/// ydotool 后端（Linux/Wayland 主流用户态输入注入，需 `ydotoold` 在跑）。
#[derive(Clone)]
pub struct YdotoolBackend {
    /// ydotool 可执行文件名（默认 `ydotool`，允许替换为绝对路径）。
    bin: String,
}

impl YdotoolBackend {
    pub fn new(bin: impl Into<String>) -> Self {
        // 对齐 ydotool 源码语义：ydotoold 在「启动期、uinput 设备建好之后」一次性把虚拟
        // 指针加速度设为 flat（`ydotoold.c` 里 `sleep(1)` 后调 xinput），不是每次发命令时。
        // 这里在构造后端（= 后端「就绪」时刻）做一次同样的一次性确保，而非放进 `run`。
        // 失败静默忽略——与上游 xinput 失败只 printf 不致命一致：未关 flat 只令相对移动
        // 落点略偏，闭环每步确认仍能收敛到正确位置，不影响最终正确性。
        let _ = ensure_ydotool_flat();
        Self { bin: bin.into() }
    }

    /// 执行一条 ydotool 子命令并校验退出码。
    fn run(&self, args: &[&str]) -> Result<()> {
        let status = std::process::Command::new(&self.bin)
            .args(args)
            .status()
            .with_context(|| format!("spawn {} 失败（确认已安装且在 PATH）", self.bin))?;
        if !status.success() {
            anyhow::bail!("ydotool 返回非零退出码 {:?}，参数 {:?}", status, args);
        }
        Ok(())
    }
}

impl InputBackend for YdotoolBackend {
    fn move_rel(&self, delta: IVec2) -> Result<()> {
        // `mousemove -- DX DY`：用 `--` 分隔符，正负增量都能正确解析（负值必须用
        // 此形式，否则 ydotool 会把负号误当选项导致光标乱跳）。
        self.run(&[
            "mousemove",
            "--",
            &delta.x.to_string(),
            &delta.y.to_string(),
        ])
        .context("ydotool 相对移动失败")
    }

    fn click(&self, btn: MouseButton) -> Result<()> {
        self.run(&["click", &format!("0x{:02X}", btn.click_code())])
            .context("ydotool 当前位置单击失败")
    }

    fn press(&self, btn: MouseButton) -> Result<()> {
        self.run(&["click", &format!("0x{:02X}", btn.down_code())])
            .context("ydotool 按下失败")
    }

    fn release(&self, btn: MouseButton) -> Result<()> {
        self.run(&["click", &format!("0x{:02X}", btn.up_code())])
            .context("ydotool 抬起失败")
    }

    fn double_click(&self, btn: MouseButton) -> Result<()> {
        let code = format!("0x{:02X}", btn.click_code());
        self.run(&["click", "-D", "60", &code, &code])
            .context("ydotool 当前位置双击失败")
    }

    fn move_abs(&self, pos: IVec2) -> Result<()> {
        // 注意：必须用 `-a -x X -y Y`，`-a` 表绝对模式。切勿写成 `mousemove -- -a X Y`，
        // 部分 ydotool 版本会 stack smashing 崩溃。本机 KWin/Wayland 下通常失效，
        // 仅作回退原语保留；闭环移动走 `ScreenOperator::ensure_move_to`。
        self.run(&[
            "mousemove",
            "-a",
            "-x",
            &pos.x.to_string(),
            "-y",
            &pos.y.to_string(),
        ])
        .context("ydotool 绝对移动失败")
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.run(&["type", text]).context("ydotool type 失败")
    }

    fn key(&self, code: u16) -> Result<()> {
        self.run(&["key", &format!("{code}:1"), &format!("{code}:0")])
            .context("ydotool key 失败")
    }

    fn key_down(&self, code: u16) -> Result<()> {
        self.run(&["key", &format!("{code}:1")])
            .context("ydotool key down 失败")
    }

    fn key_up(&self, code: u16) -> Result<()> {
        self.run(&["key", &format!("{code}:0")])
            .context("ydotool key up 失败")
    }

    fn combo(&self, codes: &[u16]) -> Result<()> {
        if codes.is_empty() {
            anyhow::bail!("combo 需要至少一个 keycode");
        }
        let mut seq: Vec<String> = Vec::with_capacity(codes.len() * 2);
        for c in codes {
            seq.push(format!("{c}:1"));
        }
        for c in codes.iter().rev() {
            seq.push(format!("{c}:0"));
        }
        let args: Vec<&str> = std::iter::once("key")
            .chain(seq.iter().map(|s| s.as_str()))
            .collect();
        self.run(&args).context("ydotool 组合键失败")
    }
}
