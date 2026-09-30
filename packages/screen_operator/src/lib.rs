//! 屏幕操作层：把「在屏幕绝对坐标上模拟人操作」这件事做成独立、可复用的 crate。
//!
//! 与 `capturer`（看）对称，构成 **看 / 操作分离** 的两条正交链路：
//! - `capturer` 负责「看」——从屏幕/窗口流拿到像素；
//! - `screen_operator` 负责「操作」——把意图（移动、点击、打字…）注入到屏幕。
//!
//! 坐标语义：**屏幕绝对坐标**（以屏幕左上角为原点）。窗口相对坐标 → 绝对坐标的
//! 换算不在本层职责内（那是 `ocr_agent` 的事，它已有 `infer_window_offset`）。
//! 本层只认绝对坐标，因此可被任何「已知道目标屏幕位置」的调用方复用，不耦合
//! 任何窗口模型。
//!
//! 注入后端：当前统一走 `ydotool`（Wayland / 通用 Linux 下的用户态输入注入，
//! 需 `ydotoold` 在跑）。ydotool 本身用绝对坐标，与本层语义天然一致。
//!
//! 模块划分：
//! - [`mouse`]：[`MouseButton`] 枚举与 ydotool 键码编码；
//! - [`keycode`]：键名 → Linux keycode 映射表与翻译函数；
//! - [`foregrounder`]：前台器 [`KdeForegrounder`]/[`Foregrounder`]（把窗口提到最前 +
//!   读 KWin 状态：窗口几何 / 屏幕逻辑尺寸 / 当前光标），是 [`probe::KwinProbe`] 的读数来源；
//! - [`injector`]：[`InputBackend`] 输入后端 trait（指针 + 键盘原语）+ 桌面实现
//!   [`input_backend::YdotoolBackend`]；
//! - [`probe`]：[`Probe`] 读数 trait + 桌面实现 [`probe::KwinProbe`]
//!   （读「当前指针在哪」）；
//! - [`operator`]：[`ScreenOperator`] 组合层：持有 `InputBackend`+`Probe`，内含「读→差→
//!   移→确认」闭环（[`ensure_move_to`]），对外只暴露直觉 API（`ensure_move_to` /
//!   `click_left_at` / `key` …）。泛型 `I: InputBackend, P: Probe`，编译期确定后端。
//!
//! **抽象边界（看 / 操作 的「操作」侧再分两层）**：
//! - [`InputBackend`] / [`Probe`] 是两个**正交** trait：输入注入（指针+键盘「怎么动」）
//!   与读数（「在哪」）彻底解耦。桌面端 = `YdotoolBackend` + `KwinProbe`；将来移动端可另写
//!   `AdbBackend` + `ScreenshotProbe`，直接喂给同一套泛型 [`ScreenOperator`]，无需重写
//!   移动/点击逻辑（闭环在组合层内，与具体后端无关）。
//! - 用**泛型** `ScreenOperator<I: InputBackend, P: Probe>` 而非枚举：开放扩展（外部 crate
//!   能加新后端），且零开销（编译期单态化，无运行时分发）。
//!
//! 已踩坑并固化在本实现里：
//! - 绝对移动必须用 `ydotool mousemove -a -x X -y Y`，**不能**用 `mousemove -- -a X Y`
//!   形式——后者在部分 ydotool 版本会触发 stack smashing（exit 134 崩溃）。
//! - 按键码：`0x40` 表按下、`0x80` 表抬起；左键完整点击 = `0x40|0x00` = `0xC0`
//!   （右 `0xC1`、中 `0xC2`），按下/抬起分离则分别只用 `0x40`/`0x80` 位。
//! - **键盘关键坑**：ydotool `key` **只认数字 keycode，不认 `KEY_*` 名字**
//!   （名字被 strtol 当成 0 静默失效）。本 crate 已内置 [`keycode::keycode_of`]
//!   把 `KEY_*` 名字翻译成数字码，调用方直接写名字即可。
//! - **本机 KWin 下 ydotool 绝对移动（`-a`）失效**：会把虚拟光标推到 (1,1) 死区。
//!   故 [`ScreenOperator::ensure_move_to`] 走相对移动闭环（相对移动可靠，
//!   单位与 KWin `cursorPos` 同为逻辑像素）+ 每步读回确认。相对移动落点**不
//!   稳定**（不可描述为固定倍率，大指令会过冲甚至撞墙），所以 `ensure_move_to` 必须靠
//!   「移动 → 读 → 确认」逐步收敛，而非预设倍率。

mod accel;
mod foregrounder;
mod input_backend;
mod keycode;
mod mouse;
mod operator;
mod probe;

pub use accel::ensure_ydotool_flat;
pub use foregrounder::{Foregrounder, KdeForegrounder, NoopForegrounder};
pub use input_backend::{InputBackend, YdotoolBackend};
pub use keycode::{KEYCODES, keycode_of};
pub use mouse::MouseButton;
pub use operator::ScreenOperator;
pub use probe::{KwinProbe, Probe};
// 坐标类型：本 crate 所有移动/点击入口统一用 `IVec2` 表达屏幕坐标 / 增量，
// 调用方直接从这里取，不必感知底层 glam 依赖。
pub use glam::IVec2;
