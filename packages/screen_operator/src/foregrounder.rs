//! 前台器（Foregrounder）：把目标窗口切到最前 + 读 KWin 状态（窗口几何 / 光标位置）。
//!
//! **为什么属于 `screen_operator`（操作侧）而非 `ocr_agent`（业务侧）**：
//! `screen_operator` 的「相对移动闭环」（`ensure_move_to` 反复「移动→读光标→确认」）
//! 必须能读当前光标逻辑坐标才能收敛；而光标读数、窗口几何都来自 KWin 的 D-Bus
//! `Scripting` 接口——本质是「操作侧的状态查询」。把读光标能力内聚进本 crate，
//! `ScreenOperator` 就能 `with_foregrounder(fg)` 自己跑闭环，不必再由上层注入一个
//! 它无法保证可靠性的 `Fn` 闭包。这不构成对「看/操作分离」的破坏：`capturer`（看）
//! 仍留在 `ocr_agent`，这里只是「操作前/操作中查询 compositor 状态」。
//!
//! 实现依赖外部命令 `qdbus6`（KWin D-Bus）与 `journalctl`（读 KWin 脚本 `print`
//! 输出），不引入新的 Rust 依赖。

use anyhow::{Context, Result};
use glam::IVec2;
use std::io::Write;

/// 前台器：把目标窗口切到最前。
///
/// 因为「切前台」是 compositor 相关动作（KDE 用 `kdotool`/D-Bus、GNOME 用 `gdbus`、
/// wlroots 用 `hyprctl` 等），跨桌面不通用，所以做成 trait 由具体实现注入；不注入
/// 时（[`NoopForegrounder`]）视为「调用方已自行保证目标在前台」。
pub trait Foregrounder {
    /// 把目标窗口切到最前。失败返回错误（如找不到窗口 / compositor 不支持）。
    fn raise(&self) -> Result<()>;
}

/// 空前台器：什么都不做。
///
/// 用于「调用方已自行保证目标窗口在前台」的场景（如人工把窗口点前台后再跑
/// 闭环），或录屏路径下明确选择「不切前台」。
pub struct NoopForegrounder;

impl Foregrounder for NoopForegrounder {
    fn raise(&self) -> Result<()> {
        Ok(())
    }
}

/// KDE (Wayland/KWin) 前台器：通过 KWin 的 D-Bus `Scripting` 接口运行一段
/// JavaScript，把标题/类名包含 `target` 的窗口 `activate()` 到最前。
///
/// 为什么用 D-Bus 而非 `kdotool`：`kdotool` 在部分 KDE 安装里没装；而 KWin 的
/// `org.kde.KWin` D-Bus 服务默认就在，且 `Scripting.loadScript + start` 能直接执行
/// 窗口管理 JS（`client.activate()` 是 KDE 窗口规则的标准能力，Wayland 下可用）。
///
/// 用法：`raise()` 时把内联 JS（含 target 变量）写到临时文件，调
/// `qdbus6 org.kde.KWin /Scripting loadScript <file> <plugin> && start`，
/// 完成后 unload 该 plugin 清理。若 KWin 未在运行 / D-Bus 不可用则报错。
///
/// 除了 `raise`，本前台器还顺带提供「读 KWin 状态」：`geometry()`（窗口几何）、
/// `screen_logical_size()`（屏幕逻辑尺寸）、`cursor_pos()`（当前光标逻辑坐标）。
/// 其中 `cursor_pos` 是 `screen_operator` 相对移动闭环的读数来源——已做脏读过滤
/// （连续读两次、差距 ≤3 才采信），规避 journalctl 紧循环里偶发的陈旧行。
///
/// 前置：`qdbus6` 在 PATH 中（Qt 工具链自带），且当前是 KDE 会话。
#[derive(Clone)]
pub struct KdeForegrounder {
    /// 窗口标题 / 类名需包含此关键字（如 "testing_08"）。
    pub target: String,
}

impl KdeForegrounder {
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }

    /// 生成 KWin 脚本内容：遍历窗口，匹配 target 则置为 active（提到最前）。
    ///
    /// 匹配策略：**标题(title)优先，应用ID(app_id)兜底**，且不受列表顺序影响——
    /// 先在整个列表里找「标题含 target」的窗口；只有完全没找到标题命中时，才退而
    /// 用「应用ID含 target」兜底。否则若应用ID命中的窗口排在标题命中的前面，会被
    /// 错误地提前锁定（已踩坑：本机 gpui 的 testing_08 标题为 "testing_08"、但另
    /// 一个 `class=testing_08 caption=""` 的窗口排在前面，导致激活到空标题那个）。
    ///
    /// 注意：本机 KWin（kwin_wayland，Plasma 6）脚本 API 与旧文档不同：
    /// - 取窗口列表是 `workspace.windowList()`（函数，非 `clientList()`）；
    /// - 激活窗口是给属性赋值 `workspace.activeWindow = c`（非 `c.activate()` /
    ///   `activateClient()`，这些在新版本里已不是函数）；
    /// - 取标题用的是 KWin 属性 `c.caption`、取应用ID用 `c.resourceClass` /
    ///   `c.resourceName`（这是 KDE 协议里的固定名，下方 JS 不可改；我们自己的
    ///   局部变量改名为 `title` / `app_id` 仅为可读性）。
    fn script(&self) -> String {
        format!(
            r#"
const target = "{}";
const wins = workspace.windowList();
let byTitle = null;    // 标题命中（优先，不受顺序影响）
let byAppId = null;    // 应用ID命中（仅当无任何标题命中时兜底）
for (let i = 0; i < wins.length; i++) {{
    const c = wins[i];
    const title = c.caption || "";                          // KWin 固定属性：窗口标题
    const app_id = c.resourceClass || c.resourceName || ""; // KWin 固定属性：应用ID
    if (title.includes(target)) {{
        byTitle = c;   // 标题命中直接记录，后面覆盖式优先
    }}
    if (!byAppId && app_id.includes(target)) {{
        byAppId = c;   // 仅在尚未记录应用ID兜底时记录
    }}
}}
const win = byTitle || byAppId;   // 标题优先，应用ID兜底
if (win) {{
    workspace.activeWindow = win;     // 设为活动
    win.keepAbove = true;             // 强制置顶（盖过其它普通窗口，如编辑器）
    workspace.raiseWindow(win);       // 提到堆叠最上
}} else {{
    throw new Error("KdeForegrounder: 未找到匹配窗口: " + target);
}}
"#,
            self.target
        )
    }

    /// 生成取目标窗口几何的 KWin 脚本：打印 `GEOMETRY_OK x=.. y=.. w=.. h=..`
    /// （`frameGeometry`，逻辑像素坐标系，含分数缩放）。供 `geometry()` 解析。
    fn geometry_script(&self) -> String {
        format!(
            r#"
const target = "{}";
const wins = workspace.windowList();
for (let i = 0; i < wins.length; i++) {{
    const c = wins[i];
    const title = c.caption || "";
    const app_id = c.resourceClass || c.resourceName || "";
    if (title.includes(target) || app_id.includes(target)) {{
        const g = c.frameGeometry;
        print("GEOMETRY_OK x=" + g.x + " y=" + g.y + " w=" + g.width + " h=" + g.height);
    }}
}}
"#,
            self.target
        )
    }

    /// 通用 KWin 脚本执行：写临时脚本（打印 `MARKER ...`）→ qdbus loadScript+start
    /// → 从 journal 读「最近 N 行里、最后一条含 `MARKER` 的行」整体返回。
    ///
    /// **为什么抓最近行而非 journal `--cursor` 边界**：在紧循环里反复调用（如
    /// `screen_operator::ensure_move_to` 每步读光标）时，`--cursor` 边界会和连续多次
    /// loadScript+start 互相竞争，常抓到旧迭代的脏行，坐标乱跳、闭环永不收敛。抓
    /// 「最近 N 行最后一条 marker」天然取到本次（或最近一次）脚本的真实输出，与
    /// 独立 `/tmp/readcur.sh` 一致。
    ///
    /// `geometry` / `cursor_pos` / `screen_logical_size` 都复用本方法，避免各自的
    /// loadScript + 读回样板重复。脚本里用 `print(MARKER + " ...")` 输出，这里返回
    /// `MARKER` 之后的那段文本供调用方解析。
    fn run_kwin_script(&self, js: &str, marker: &str) -> Result<String> {
        // 1. 写临时脚本并 load + start。
        let mut path = std::env::temp_dir();
        path.push(format!("ocr_lab_{}_{}.js", marker, std::process::id()));
        {
            let mut f = std::fs::File::create(&path)
                .with_context(|| format!("写 KWin 脚本失败: {}", path.display()))?;
            f.write_all(js.as_bytes())
                .with_context(|| "写 KWin 脚本内容失败")?;
        }
        let plugin = format!("ocr_lab_{}", marker);
        let _ = std::process::Command::new("qdbus6")
            .args(["org.kde.KWin", "/Scripting", "unloadScript", &plugin])
            .output();
        let load = std::process::Command::new("qdbus6")
            .args([
                "org.kde.KWin",
                "/Scripting",
                "loadScript",
                &path.to_string_lossy(),
                &plugin,
            ])
            .output()
            .with_context(|| format!("调 qdbus6 loadScript({}) 失败", marker))?;
        if !load.status.success() {
            anyhow::bail!(
                "KWin loadScript({}) 失败: {}",
                marker,
                String::from_utf8_lossy(&load.stderr)
            );
        }
        let start = std::process::Command::new("qdbus6")
            .args(["org.kde.KWin", "/Scripting", "start"])
            .output()
            .with_context(|| format!("调 qdbus6 start({}) 失败", marker))?;
        if !start.status.success() {
            anyhow::bail!(
                "KWin start({}) 失败: {}",
                marker,
                String::from_utf8_lossy(&start.stderr)
            );
        }
        let _ = std::fs::remove_file(&path);
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 2. 读 marker 行（抓最近 N 行最后一条，详见上方文档）。
        let out = std::process::Command::new("journalctl")
            .args(["_COMM=kwin_wayland", "-n", "30", "--no-pager"])
            .output()
            .with_context(|| "调 journalctl 读脚本输出失败")?;
        let text = String::from_utf8_lossy(&out.stdout);
        // 从后往前找，取最近一条 marker 行（tail 语义）。
        let found = text
            .lines()
            .rev()
            .find_map(|line| line.split(marker).nth(1).map(str::trim));
        match found {
            Some(rest) => Ok(rest.to_string()),
            None => anyhow::bail!("未从 KWin 日志解析到 {} 输出", marker),
        }
    }

    /// 取目标窗口在屏幕上的几何 `(x, y, w, h)`（逻辑像素，compositor 坐标系），
    /// 用于把「全屏截图中裁剪出的窗口内容」对齐到屏幕绝对坐标去点击。
    pub fn geometry(&self) -> Result<(i32, i32, i32, i32)> {
        let rest = self.run_kwin_script(&self.geometry_script(), "GEOMETRY_OK")?;
        // rest 形如 " x=411.84 y=447.61 w=308 h=236.4"
        let parse = |key: &str| -> Option<f64> {
            let idx = rest.find(key)?;
            let after = &rest[idx + key.len()..];
            let val: String = after
                .chars()
                .skip_while(|c| c.is_whitespace() || *c == '=')
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            val.parse::<f64>().ok()
        };
        if let (Some(x), Some(y), Some(w), Some(h)) =
            (parse("x="), parse("y="), parse("w="), parse("h="))
        {
            return Ok((
                x.round() as i32,
                y.round() as i32,
                w.round() as i32,
                h.round() as i32,
            ));
        }
        anyhow::bail!(
            "未从 KWin 日志解析到窗口几何（目标 {} 未运行？）",
            self.target
        )
    }

    /// 取整个屏幕的**逻辑像素**尺寸 `(w, h)`（compositor 坐标系，含分数缩放）。
    ///
    /// 用途：配合全屏 Monitor 截图的**物理**像素尺寸，得到全局缩放
    /// `scale = 全屏物理宽 / 屏幕逻辑宽`——这才是屏幕真实缩放，比用「窗口流 buffer 宽 /
    /// 窗口逻辑宽」更稳（窗口 buffer 可能有自身取整噪声，且不一定是全局缩放）。
    /// 数据来自 KWin 脚本 `workspace.screens[0].geometry`（系统真值，非硬编码常数）。
    pub fn screen_logical_size(&self) -> Result<(i32, i32)> {
        let js = "print(\"DISPLAY_OK \" + workspace.screens[0].geometry.width + \" \" + workspace.screens[0].geometry.height);\n";
        let rest = self.run_kwin_script(js, "DISPLAY_OK")?;
        let mut it = rest.split_whitespace();
        if let (Some(ws), Some(hs)) = (it.next(), it.next()) {
            if let (Ok(w), Ok(h)) = (ws.trim().parse::<f64>(), hs.trim().parse::<f64>()) {
                return Ok((w.round() as i32, h.round() as i32));
            }
        }
        anyhow::bail!("未从 KWin 日志解析到屏幕尺寸")
    }

    /// 取当前鼠标光标的**逻辑像素**坐标 `(x, y)`（与 KWin `cursorPos`、本机分数缩放
    /// 下的逻辑坐标系一致）。
    ///
    /// 用途：本机 ydotool 的**绝对移动（`mousemove -a`）失效**，只能靠相对移动
    /// （`screen_operator::move_rel`）模拟绝对定位。模拟时先调本方法读当前光标逻辑
    /// 坐标，再 `move_rel(目标 - 当前)` 过去。注意绝对移动失败会导致 KWin 读到的坐标
    /// 不更新，但**真实鼠标移动 / 相对移动后 KWin 读数是准的**，故本方法在相对移动后
    /// 可正确反映光标实际位置。
    ///
    /// **脏读过滤**：`run_kwin_script` 走 journalctl 抓最近行，极偶发（脚本刚启动、
    /// 上一条旧输出尚未被冲刷时）会读到陈旧坐标。这里连续读两次、间隔 40ms，若两次
    /// 一致（或差距 ≤ 容差）则采信；不一致说明正处在 race 窗口，再读一次取最新。这样
    /// 喂给 `screen_operator::ensure_move_to` 的读数稳定，闭环不会基于脏数据狂发移动。
    pub fn cursor_pos(&self) -> Result<IVec2> {
        const RETRY: usize = 3;
        const STABLE_TOL: i32 = 3; // 两次读数差距 ≤ 此值视为稳定。
        let mut last: Option<IVec2> = None;
        for i in 0..RETRY {
            let rest = self.run_kwin_script(
                "print(\"CURSOR \" + workspace.cursorPos.x + \" \" + workspace.cursorPos.y);\n",
                "CURSOR ",
            )?;
            let mut it = rest.split_whitespace();
            let parsed = (|| {
                let (xs, ys) = (it.next()?, it.next()?);
                let x = xs.trim().parse::<f64>().ok()?;
                let y = ys.trim().parse::<f64>().ok()?;
                let pos = IVec2::new(x.round() as i32, y.round() as i32);
                Some(pos)
            })();
            match parsed {
                None => {
                    if i + 1 < RETRY {
                        std::thread::sleep(std::time::Duration::from_millis(40));
                        continue;
                    }
                    anyhow::bail!("未从 KWin 日志解析到光标坐标");
                }
                Some(p) => {
                    if let Some(prev) = last {
                        if (p.x - prev.x).abs() <= STABLE_TOL && (p.y - prev.y).abs() <= STABLE_TOL
                        {
                            return Ok(p); // 两次稳定，采信。
                        }
                    }
                    last = Some(p);
                    if i + 1 < RETRY {
                        std::thread::sleep(std::time::Duration::from_millis(40));
                    } else {
                        return Ok(p); // 重试耗尽，用最后一次。
                    }
                }
            }
        }
        anyhow::bail!("未从 KWin 日志解析到光标坐标")
    }
}

impl Foregrounder for KdeForegrounder {
    fn raise(&self) -> Result<()> {
        // 1. 写临时脚本文件。
        let mut path = std::env::temp_dir();
        path.push(format!("ocr_lab_raise_{}.js", std::process::id()));
        {
            let mut f = std::fs::File::create(&path)
                .with_context(|| format!("写 KWin 脚本失败: {}", path.display()))?;
            f.write_all(self.script().as_bytes())
                .with_context(|| "写 KWin 脚本内容失败")?;
        }
        let plugin = "ocr_lab_raise";
        // 2. 先尝试 unload 旧实例（忽略失败），再 load + start。
        let _ = std::process::Command::new("qdbus6")
            .args(["org.kde.KWin", "/Scripting", "unloadScript", plugin])
            .output();
        let load = std::process::Command::new("qdbus6")
            .args([
                "org.kde.KWin",
                "/Scripting",
                "loadScript",
                &path.to_string_lossy(),
                plugin,
            ])
            .output()
            .with_context(|| "调 qdbus6 loadScript 失败（确认 qdbus6 在 PATH 且为 KDE 会话）")?;
        if !load.status.success() {
            anyhow::bail!(
                "KWin loadScript 失败: {}",
                String::from_utf8_lossy(&load.stderr)
            );
        }
        let start = std::process::Command::new("qdbus6")
            .args(["org.kde.KWin", "/Scripting", "start"])
            .output()
            .with_context(|| "调 qdbus6 start 失败")?;
        if !start.status.success() {
            anyhow::bail!(
                "KWin start 失败: {}",
                String::from_utf8_lossy(&start.stderr)
            );
        }
        // 3. 清理临时文件（KWin 已读入内容）。
        let _ = std::fs::remove_file(&path);
        // 4. 让 KWin 完成 activate()（raise + focus）后再返回，否则紧接着的
        //    截屏仍会抓到旧的最前窗口。实际是异步重绘，这里稍作等待。
        std::thread::sleep(std::time::Duration::from_millis(350));
        Ok(())
    }
}
