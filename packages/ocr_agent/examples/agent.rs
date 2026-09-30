//! 命令行：演示 `ocr_agent` 对 testing_08 的「识别 → 定位 → 点击」链路，
//! 并演示**自动反推窗口偏移**（无需 xdotool/kdotool，纯图像方法）与**闭环验证**。
//!
//! 用法：
//!   # 默认：PrintExecutor（dry-run），手动无偏移。
//!   cargo run -p ocr_agent --example agent -- <image.png>
//!
//!   # 自动反推偏移（离线双图）：用已有的全屏图 + 窗口流图反推屏幕偏移。
//!   cargo run -p ocr_agent --example agent -- <window.png> \
//!       --auto-offset --full <fullscreen.png> --window <window.png>
//!
//!   # 结合真实点击：反推出偏移后用 ydotool 真点。
//!   cargo run -p ocr_agent --example agent -- <window.png> \
//!       --auto-offset --full <full.png> --window <window.png> --real
//!
//!   # 闭环验证：直接对比「点击前 / 点击后」两帧的计数（两帧由你在点击前后各
//!   # 抓一次，如用 pw_probe）。delta 即本次操作带来的计数变化。
//!   cargo run -p ocr_agent --example agent --verify-before <before.png> --verify-after <after.png>
//!
//!   # 真·自动闭环（live，ScreenCast 窗口流 + KWin 几何方案）：用 portal 的窗口流
//!   # 抓 testing_08 自身合成表面（遮挡无关、不含 portal 浮层、本机窗口捕获已预授权
//!   # 不弹阻塞对话框）→ 识别 Reload 按钮「窗口相对坐标」；点击的绝对坐标由
//!   # KdeForegrounder::geometry() 经 KWin 拿到的窗口屏幕位置换算 → ydotool 点 →
//!   # 再抓窗口流 → 比 delta。需确保只运行一个 testing_08（同名多实例会选窗不确定）。
//!   # 需：ydotoold 已起（systemctl --user enable --now ydotool.service）、
//!   #    testing_08 进程在跑。
//!   # dry-run（验证识别/定位链路，不真点）：
//!   cargo run -p ocr_agent --example agent --live --label Reload
//!   # 真点（点 Reload 后 count 应 +50）：
//!   cargo run -p ocr_agent --example agent --live --real --label Reload
//!
//! 说明：PrintExecutor 不会真点，所以前后 count 不变；--live --real 用
//! YdotoolExecutor + KdeForegrounder 真点，点 Reload 后 count 应 +50。
//!
//! 注：capturer 的 ScreenCast 后端走 ashpd + PipeWire；ocr_agent 的窗口位置来自 KWin
//! D-Bus 脚本（本机 ScreenCast 的 Stream::position() 返回 None，故改走 KWin）。早期
//! Screenshot 接口因授权窗自动关闭已弃用。

use anyhow::Context as _;
use capturer::{Capturer, ScreenCastCapturer};
use ocr_agent::{
    Agent, Executor, Foregrounder, PrintExecutor, YdotoolExecutor, infer_window_offset,
};
use ocr_layout::LayoutAnalyzer;
use rapidocr_ort::ModelProfile;
use screen_operator::{IVec2, MouseButton, ScreenOperator};
use std::path::PathBuf;
use tracing_subscriber;

/// restore_token 持久化文件（位于仓库根下的 `.cache/`），实现「提前赋权」：
/// 首次手动选屏/选窗得到 token 后写盘，之后运行直接读盘复用，不再弹对话框。
/// 全屏与窗口是不同源，token 不能串用，故分两个文件分别存。
fn restore_token_path(source: &str) -> PathBuf {
    repo_root()
        .join(".cache")
        .join(format!("screencast_restore_token_{}", source))
}

/// 读取已持久化的 token（monitor / window），返回 (monitor, window)。
fn load_tokens() -> (Option<String>, Option<String>) {
    let read = |s: &str| {
        let p = restore_token_path(s);
        if p.exists() {
            std::fs::read_to_string(&p)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        } else {
            None
        }
    };
    (read("monitor"), read("window"))
}

/// 写盘持久化某个源的 token（提前赋权）。
fn save_token(source: &str, token: &str) -> anyhow::Result<()> {
    let p = restore_token_path(source);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("建目录失败: {}", parent.display()))?;
    }
    std::fs::write(&p, token).with_context(|| format!("写 token 失败: {}", p.display()))?;
    Ok(())
}

fn repo_root() -> PathBuf {
    let exe = std::env::current_exe().expect("当前可执行文件路径");
    exe.parent()
        .unwrap()
        .join("..") // target/debug
        .join("..") // crates/ocr_agent
        .join("..") // 仓库根
        .canonicalize()
        .expect("解析仓库根失败")
}

/// 把 OCR 标注图保存到仓库根下的 `tmp/` 目录（用于人工核对「看」得对不对）。
fn save_annotated(name: &str, img: &image::RgbImage) -> anyhow::Result<PathBuf> {
    let dir = repo_root().join("tmp");
    std::fs::create_dir_all(&dir).with_context(|| format!("建 tmp 目录失败: {}", dir.display()))?;
    let path = dir.join(name);
    img.save(&path)
        .with_context(|| format!("保存标注图失败: {}", path.display()))?;
    Ok(path)
}

fn main() -> anyhow::Result<()> {
    // 安装 tracing 订阅器：用 RUST_LOG=debug（或 RUST_LOG=screen_operator=debug）开启
    // 移动每步调试打印；不设则所有 tracing 事件静默丢弃。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!(
            "用法: agent <image.png> [--auto-offset --full F --window W] [--real] | agent --verify-before B --verify-after A"
        );
    }

    let mut auto_offset = false;
    let mut full_path: Option<String> = None;
    let mut window_path: Option<String> = None;
    let mut real = false;
    let mut verify_before: Option<String> = None;
    let mut verify_after: Option<String> = None;
    let mut live = false;
    let mut live_label = "Reload".to_string();
    let mut dump = false;
    let mut dump_stream = false;
    let mut dump_both = false;
    let mut click_abs: Option<(i32, i32)> = None;
    let mut click_reload = false;
    let mut debug = false;
    let mut double_current = false;
    let mut click_once = false;
    let mut move_abs: Option<(i32, i32)> = None;
    let mut move_to_reload = false;
    let mut move_only = false;

    let mut i = 1; // 从 1 开始，跳过程序名（首参可能是 image 或 --verify-before）
    while i < args.len() {
        match args[i].as_str() {
            "--auto-offset" => auto_offset = true,
            "--full" => full_path = args.get(i + 1).cloned(),
            "--window" => window_path = args.get(i + 1).cloned(),
            "--real" => real = true,
            "--verify-before" => verify_before = args.get(i + 1).cloned(),
            "--verify-after" => verify_after = args.get(i + 1).cloned(),
            "--live" => live = true,
            "--label" => live_label = args.get(i + 1).cloned().unwrap_or_else(|| "Reload".into()),
            "--dump" => dump = true,
            "--dump-stream" => dump_stream = true,
            "--dump-both" => dump_both = true,
            "--click-abs" => {
                // 后面跟两个数字：--click-abs X Y
                let x = args.get(i + 1).and_then(|s| s.parse::<i32>().ok());
                let y = args.get(i + 2).and_then(|s| s.parse::<i32>().ok());
                if let (Some(x), Some(y)) = (x, y) {
                    click_abs = Some((x, y));
                } else {
                    anyhow::bail!("--click-abs 需要两个数字参数：--click-abs X Y");
                }
            }
            "--click-reload" => click_reload = true,
            "--debug" => debug = true,
            "--double-current" => double_current = true,
            "--click-current" => click_once = true,
            "--move-to-reload" => move_to_reload = true,
            "--move-only" => move_only = true,
            "--move-abs" => {
                let x = args.get(i + 1).and_then(|s| s.parse::<i32>().ok());
                let y = args.get(i + 2).and_then(|s| s.parse::<i32>().ok());
                if let (Some(x), Some(y)) = (x, y) {
                    move_abs = Some((x, y));
                } else {
                    anyhow::bail!("--move-abs 需要两个数字：--move-abs X Y");
                }
            }
            _ => {}
        }
        i += 1;
    }

    // ---- 诊断：raise testing_08 → 截全屏 → OCR 列出所有控件 ----
    // 用来确认「切前台 + 截屏」到底有没有真的抓到 testing_08（而不是被别的窗口盖住）。
    if dump {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let mut analyzer =
            LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
                .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;
        // 先切前台（即使 dry-run 也切，便于诊断）。
        let fg = ocr_agent::KdeForegrounder::new("testing_08");
        fg.raise().context("KdeForegrounder::raise 失败")?;
        std::thread::sleep(std::time::Duration::from_millis(400));
        let rgba = async_io::block_on(capturer::capture_screenshot()).context("截全屏失败")?;
        let img = ocr_agent::rgba_to_rgb(&rgba);
        let widgets = analyzer.analyze(&img)?;
        eprintln!("=== OCR 共识别 {} 个控件 ===", widgets.len());
        for (idx, w) in widgets.iter().enumerate() {
            let (x, y, ww, hh) = w.rect;
            eprintln!(
                "#{} label={:?} area={:.2}% rect=({},{},{},{})",
                idx,
                w.label,
                w.area_ratio * 100.0,
                x,
                y,
                ww,
                hh
            );
        }
        return Ok(());
    }

    // ---- 诊断：抓「窗口流」这一帧并 OCR 标注存盘，让你看 ocr_agent 是怎么「看」的 ----
    // 这是 live 闭环里真正用来识别的同一帧（testing_08 自身合成表面，窗口相对坐标）。
    // 标注图含每个控件的边框 + 中心十字 + 文本标签，存到 tmp/annotated_window.png。
    if dump_stream {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let mut analyzer =
            LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
                .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;

        let (_saved_mon, saved_win) = load_tokens();
        let cap = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let (rgba, _pos, new_token) = async_io::block_on(cap.capture_app_geom(""))
            .context("抓窗口流失败（首次可能需手动选窗授权）")?;
        if let Some(t) = &new_token {
            save_token("window", t)?;
            eprintln!("已保存窗口 token，下次运行不再弹选窗对话框");
        }
        let img = ocr_agent::rgba_to_rgb(&rgba);
        eprintln!(
            "窗口流帧尺寸 {}x{}（此为窗口相对坐标空间）",
            img.width(),
            img.height()
        );

        let widgets = analyzer.analyze(&img)?;
        eprintln!("=== 窗口流 OCR 共识别 {} 个控件 ===", widgets.len());
        for (idx, w) in widgets.iter().enumerate() {
            let (x, y, ww, hh) = w.rect;
            eprintln!(
                "#{} label={:?} area={:.2}% rect=({},{},{},{}) center=({},{})",
                idx,
                w.label,
                w.area_ratio * 100.0,
                x,
                y,
                ww,
                hh,
                x + ww / 2,
                y + hh / 2
            );
        }

        let annotated = ocr_layout::annotate(&img, &widgets);
        let path = save_annotated("annotated_window.png", &annotated)?;
        eprintln!("已保存标注图 -> {}", path.display());

        // 同时存一张「未标注原图」，便于对比。
        let raw = save_annotated("window_raw.png", &img)?;
        eprintln!("已保存原图 -> {}", raw.display());
        return Ok(());
    }

    // ---- 诊断：抓「两帧」全屏+窗口流（间隔 1s，优先第二帧），在全屏上标出定位的
    // app 位置，仅供你看「看 + 定位」对不对，不跑点击全程。----
    // 默认闭环也用「两帧」：第一帧可能有过渡/特殊情况，故优先用第二帧做识别与定位。
    if dump_both {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let mut analyzer =
            LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
                .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;

        let (saved_mon, saved_win) = load_tokens();
        let cap_full = match &saved_mon {
            Some(t) => ScreenCastCapturer::with_monitor_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let cap_win = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        // 抓完两帧后，把新拿到的 token 持久化（首次弹窗后生效，之后免弹）。
        let mut new_mon: Option<String> = None;
        let mut new_win: Option<String> = None;

        // 抓两帧（每帧 = 全屏 + 窗口流），帧间间隔 1s。
        let mut frames = Vec::new();
        for f in 0..2 {
            let (full_rgba, tok_m) = async_io::block_on(cap_full.capture_fullscreen_token())
                .context("抓全屏失败（首次可能需手动选屏授权）")?;
            if tok_m.is_some() {
                new_mon = tok_m;
            }
            let (win_rgba, _pos, tok_w) = async_io::block_on(cap_win.capture_app_geom(""))
                .context("抓窗口流失败（首次可能需手动选窗授权）")?;
            if tok_w.is_some() {
                new_win = tok_w;
            }
            frames.push((
                ocr_agent::rgba_to_rgb(&full_rgba),
                ocr_agent::rgba_to_rgb(&win_rgba),
            ));
            if f == 0 {
                eprintln!("[帧1] 已抓，等待 1s 再抓第二帧…");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        // 持久化本次新拿到的 token（首次弹窗后写入，之后复用免弹）。
        if let Some(t) = &new_mon {
            save_token("monitor", t)?;
            eprintln!("已保存全屏 token，下次运行不再弹选屏对话框");
        }
        if let Some(t) = &new_win {
            save_token("window", t)?;
            eprintln!("已保存窗口 token，下次运行不再弹选窗对话框");
        }

        // 优先用第二帧（frames[1]）做定位与标注；第一帧也存一份供对比。
        let (full0, win0) = &frames[0];
        let (full_img, win_img) = &frames[1];
        let (ox, oy) = ocr_agent::infer_window_offset(full_img, win_img);
        eprintln!(
            "[第二帧] 反推窗口位置 offset=(x={}, y={})；窗口尺寸 {}x{}",
            ox,
            oy,
            win_img.width(),
            win_img.height()
        );

        // 窗口流 OCR 标注（看「识别」）——用第二帧。
        let widgets = analyzer.analyze(win_img)?;
        eprintln!("[第二帧] 窗口流 OCR 共识别 {} 个控件", widgets.len());
        for (idx, w) in widgets.iter().enumerate() {
            let (x, y, ww, hh) = w.rect;
            eprintln!(
                "  #{} label={:?} center=({},{})",
                idx,
                w.label,
                x + ww / 2,
                y + hh / 2
            );
        }
        let annotated_win = ocr_layout::annotate(win_img, &widgets);
        let p1 = save_annotated("both_window_annotated.png", &annotated_win)?;
        eprintln!("已保存窗口标注图(第二帧) -> {}", p1.display());

        // 在第二帧全屏上画红框标出 app 位置（看「定位」）；并标出 Reload 中心的
        // 绝对点击点（十字），方便你核对点的落点是否真在按钮上。
        let mut full_mark = full_img.clone();
        let (ww, wh) = (win_img.width(), win_img.height());
        let x0 = ox.max(0) as u32;
        let y0 = oy.max(0) as u32;
        let x1 = (x0 + ww).min(full_mark.width());
        let y1 = (y0 + wh).min(full_mark.height());
        for xx in x0..x1 {
            for yy in [y0, y1.saturating_sub(1)] {
                if yy < full_mark.height() {
                    *full_mark.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        for yy in y0..y1 {
            for xx in [x0, x1.saturating_sub(1)] {
                if xx < full_mark.width() {
                    *full_mark.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        // 标出 Reload 中心绝对落点（若 OCR 命中）。
        if let Some(r) = widgets.iter().find(|w| w.label == "Reload") {
            let (rx, ry, rww, rhh) = r.rect;
            let ax = (ox + rx as i32 + rww as i32 / 2).max(0) as u32;
            let ay = (oy + ry as i32 + rhh as i32 / 2).max(0) as u32;
            for d in 0..12u32 {
                for (px, py) in [
                    (ax.saturating_sub(d), ay),
                    (ax + d, ay),
                    (ax, ay.saturating_sub(d)),
                    (ax, ay + d),
                ] {
                    if px < full_mark.width() && py < full_mark.height() {
                        *full_mark.get_pixel_mut(px, py) = image::Rgb([0, 255, 0]);
                    }
                }
            }
            eprintln!(
                "[第二帧] Reload 绝对落点 ≈ ({}, {})（绿十字，应在红框内按钮处）",
                ax, ay
            );
        }
        let p2 = save_annotated("both_fullscreen_with_window.png", &full_mark)?;
        eprintln!(
            "已保存全屏标注图(第二帧,红框=app,绿十字=Reload落点) -> {}",
            p2.display()
        );

        // 顺便存第一帧全屏标注，供你对比第一帧是否「有特殊情况」。
        let (ox0, oy0) = ocr_agent::infer_window_offset(full0, win0);
        let mut full0_mark = full0.clone();
        let x0a = ox0.max(0) as u32;
        let y0a = oy0.max(0) as u32;
        let x1a = (x0a + ww).min(full0_mark.width());
        let y1a = (y0a + wh).min(full0_mark.height());
        for xx in x0a..x1a {
            for yy in [y0a, y1a.saturating_sub(1)] {
                if yy < full0_mark.height() {
                    *full0_mark.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        for yy in y0a..y1a {
            for xx in [x0a, x1a.saturating_sub(1)] {
                if xx < full0_mark.width() {
                    *full0_mark.get_pixel_mut(xx, yy) = image::Rgb([255, 0, 0]);
                }
            }
        }
        let p0 = save_annotated("both_fullscreen_frame1.png", &full0_mark)?;
        eprintln!("已保存全屏标注图(第一帧,红框=app) -> {}", p0.display());
        return Ok(());
    }

    // ---- 探针：在「当前鼠标位置」原地**单击**（不移动鼠标）----
    // 比 --double-current 更细粒度，用于确认「原地注入」本身会不会导致鼠标跳行。
    if click_once {
        let op = ScreenOperator::new();
        eprintln!("screen_operator 在原地点击当前鼠标位置（单击）…");
        op.click(MouseButton::Left).context("原地单击失败")?;
        eprintln!("已原地单击（不移动鼠标）");
        return Ok(());
    }

    // ---- 探针：在「当前鼠标位置」原地双击（不移动鼠标）----
    // 用于验证 screen_operator 的注入本身有没有效：你把鼠标移到目标处（如编辑器里
    // 某行），运行此命令，看原地点两下是否触发预期响应（如选中词 / 展开）。
    if double_current {
        let op = ScreenOperator::new();
        eprintln!("screen_operator 在原地双击当前鼠标位置…");
        op.double_click(MouseButton::Left).context("原地双击失败")?;
        eprintln!("已原地双击（不移动鼠标）");
        return Ok(());
    }

    // ---- 探针：只移动鼠标到绝对坐标（不点击，用相对移动模式）----
    // 验证「逻辑绝对定位」是否正确：读当前 KWin 逻辑坐标，相对移动到目标。
    // 入参 (x, y) 为**物理**坐标，先 ÷scale 转逻辑（ScreenOperator 入口收逻辑）。
    if let Some((x, y)) = move_abs {
        let fg_probe = ocr_agent::KdeForegrounder::new("testing_08");
        let scale = fg_probe
            .geometry()
            .ok()
            .map(|(_, _, lw, _)| 493.0f32 / lw.max(1) as f32)
            .unwrap_or(1.0);
        let op = ScreenOperator::new();
        let c_pos = fg_probe.cursor_pos().context("读当前光标失败")?;
        let tx = (x as f32 / scale).round() as i32;
        let ty = (y as f32 / scale).round() as i32;
        let t_pos = IVec2::new(tx, ty);
        let delta = t_pos - c_pos;
        eprintln!(
            "相对移动：当前逻辑({c_pos}), 目标逻辑({t_pos}), 增量({delta})（不点击, scale={:.3}）",
            scale
        );
        op.ensure_move_to(t_pos).context("移动鼠标失败")?;
        eprintln!("已移动（请观察鼠标实际落点；可再用 KWin 读坐标核对）");
        return Ok(());
    }

    // ---- 单独验证「操作层」screen_operator：直接点指定绝对坐标 ----
    // 不抓图、不闭环，仅验证「定位点击」整条链（相对移动 + 点击）能不能点中目标。
    // 入参 (x, y) 为**物理**坐标，先 ÷scale 转逻辑再交给 ScreenOperator（其入口收逻辑）。
    // 例：cargo run -p ocr_agent --example agent -- --click-abs 1081 986
    if let Some((x, y)) = click_abs {
        // scale 取全局分数缩放：从 testing_08 的 KWin 几何推算（窗口流宽 / 逻辑宽）。
        let fg_probe = ocr_agent::KdeForegrounder::new("testing_08");
        let scale = fg_probe
            .geometry()
            .ok()
            .map(|(_, _, lw, _)| 493.0f32 / lw.max(1) as f32)
            .unwrap_or(1.0);
        let op = ScreenOperator::new().with_foregrounder(fg_probe);
        let (lx, ly) = (
            (x as f32 / scale).round() as i32,
            (y as f32 / scale).round() as i32,
        );
        let l_pos = IVec2::new(lx, ly);

        eprintln!(
            "screen_operator 用相对移动模式点击逻辑坐标 ({l_pos})（物理 {}, {}, scale={:.3}）",
            x, y, scale
        );
        op.click_left_at(l_pos)
            .context("screen_operator 点击失败")?;
        eprintln!("已点击逻辑 ({}, {})", lx, ly);
        return Ok(());
    }

    // ---- 端到端验证「看 + 定位 + 操作」：抓两帧算当前 Reload 绝对坐标，
    // 再用 screen_operator 点它（不读 count、不判 delta，只验证点得准不准）。----
    if click_reload {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let mut analyzer =
            LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
                .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;

        // 先把目标提到最前（Wayland 输入要求点击点最上层）。
        let fg = ocr_agent::KdeForegrounder::new("testing_08");
        fg.raise().context("把 testing_08 提到最前失败")?;

        let (saved_mon, saved_win) = load_tokens();
        let cap_full = match &saved_mon {
            Some(t) => ScreenCastCapturer::with_monitor_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let cap_win = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };

        // 抓两帧（间隔 1s，优先第二帧），用第二帧反推 offset + 找 Reload。
        let mut frames = Vec::new();
        for f in 0..2 {
            let (full_rgba, tok_m) = async_io::block_on(cap_full.capture_fullscreen_token())
                .context("抓全屏失败（首次可能需手动选屏授权）")?;
            if tok_m.is_some() {
                save_token("monitor", &tok_m.unwrap())?;
            }
            let (win_rgba, _pos, tok_w) = async_io::block_on(cap_win.capture_app_geom(""))
                .context("抓窗口流失败（首次可能需手动选窗授权）")?;
            if tok_w.is_some() {
                save_token("window", &tok_w.unwrap())?;
            }
            frames.push((
                ocr_agent::rgba_to_rgb(&full_rgba),
                ocr_agent::rgba_to_rgb(&win_rgba),
            ));
            if f == 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        let (_full0, _win0) = &frames[0];
        let (full_img, win_img) = &frames[1];
        let (ox, oy) = ocr_agent::infer_window_offset(full_img, win_img);
        let widgets = analyzer.analyze(win_img)?;
        let reload = widgets
            .iter()
            .find(|w| w.label == "Reload")
            .context("窗口流里没找到 Reload 控件")?;
        let (rx, ry, rww, rhh) = reload.rect;
        let ax = ox + rx as i32 + rww as i32 / 2;
        let ay = oy + ry as i32 + rhh as i32 / 2;
        eprintln!(
            "当前 Reload 绝对坐标 ≈ ({}, {})（offset={:?}, 窗口内中心=({},{})）",
            ax,
            ay,
            (ox, oy),
            rx + rww / 2,
            ry + rhh / 2
        );

        // ax, ay 是物理坐标，转逻辑后交给 ScreenOperator（其入口收逻辑坐标）。
        // 用相对移动模式（绕开本机失效的 ydotool 绝对移动）。
        let fg_live = ocr_agent::KdeForegrounder::new("testing_08");
        let scale_live = fg_live
            .geometry()
            .ok()
            .map(|(_, _, lw, _)| 493.0f32 / lw.max(1) as f32)
            .unwrap_or(1.0);
        let (lax, lay) = (
            (ax as f32 / scale_live).round() as i32,
            (ay as f32 / scale_live).round() as i32,
        );
        let la_pos = IVec2::new(lax, lay);
        let op = ScreenOperator::new().with_foregrounder(fg_live);
        op.click_left_at(la_pos)
            .context("screen_operator 点击 Reload 失败")?;
        eprintln!("screen_operator 已点击 Reload 逻辑({la_pos})",);
        return Ok(());
    }

    // ---- 探针：只把鼠标移到 Reload（不点击），验证「移动能否精准落到按钮」----
    // 用 KWin 几何(稳定真值)算 offset + 窗口流 OCR 找 Reload 中心，相对移动模式移过去，
    // 不点击。你肉眼确认光标是否落在 Reload 按钮上；同时用 KWin 读实际落点打印出来。
    if move_to_reload {
        let fg = ocr_agent::KdeForegrounder::new("testing_08");
        fg.raise().context("raise 失败")?;
        let (saved_mon, saved_win) = load_tokens();
        let cap_win = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let (win_rgba, _pos, tok_w) = async_io::block_on(cap_win.capture_app_geom(""))?;
        if tok_w.is_some() {
            save_token("window", &tok_w.unwrap())?;
        }
        let win_img = ocr_agent::rgba_to_rgb(&win_rgba);
        let (lx, ly, lw, _lh) = fg.geometry()?;
        let scale = win_img.width() as f32 / lw.max(1) as f32;
        let gx = (lx as f32 * scale) as i32;
        let gy = (ly as f32 * scale) as i32;
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let widgets = LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())?
            .analyze(&win_img)?;
        let reload = widgets
            .iter()
            .find(|w| w.label == "Reload")
            .context("窗口流里没找到 Reload 控件")?;
        let (rx, ry, rww, rhh) = reload.rect;
        let ax = gx + rx as i32 + rww as i32 / 2;
        let ay = gy + ry as i32 + rhh as i32 / 2;
        eprintln!(
            "Reload 绝对坐标(物理) ≈ ({}, {}) [offset=({},{}) scale={:.3}, 窗口内中心=({},{}), OCR识别到 {} 个控件]",
            ax,
            ay,
            gx,
            gy,
            scale,
            rx + rww / 2,
            ry + rhh / 2,
            widgets.len()
        );
        // ax, ay 是物理坐标，转成逻辑再交给 ensure_move_to（其入口收逻辑坐标）。
        let (lax, lay) = (
            (ax as f32 / scale).round() as i32,
            (ay as f32 / scale).round() as i32,
        );
        let la_pos = IVec2::new(lax, lay);
        let op = ScreenOperator::new().with_foregrounder(fg.clone());
        op.ensure_move_to(la_pos).context("移动失败")?;
        eprintln!("已移动（未点击）；请肉眼确认光标是否在 Reload 上");
        return Ok(());
    }

    // ---- 移动探针（复用闭环同款定位，但只移动不点）----
    // 与 --live 完全一致的 raise→抓帧→KWin几何算offset→窗口流OCR找 Reload 中心，
    // 只是最后调用 ensure_move_to（移动不点击）而非 click_widget。用于隔离「闭环里移动
    // 是否过冲」：你肉眼确认光标是否落在 Reload 上，并比对与 --move-to-reload 的差异。
    if move_only {
        let fg = ocr_agent::KdeForegrounder::new("testing_08");
        fg.raise().context("raise 失败")?;
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let mut analyzer =
            LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
                .context("构建 OCR 引擎失败")?;
        let (_saved_mon, saved_win) = load_tokens();
        let cap = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let (win_rgba, _pos, new_token) = async_io::block_on(cap.capture_app_geom(""))?;
        if let Some(t) = &new_token {
            save_token("window", t)?;
        }
        let win_img = ocr_agent::rgba_to_rgb(&win_rgba);
        // 全屏流：用其物理尺寸 + 屏幕逻辑尺寸算全局 scale（见 --live 注释）。
        let (_saved_mon, _saved_win2) = load_tokens();
        let cap_full = match &_saved_mon {
            Some(t) => ScreenCastCapturer::with_monitor_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let full_rgba = async_io::block_on(cap_full.capture_fullscreen())?;
        let full_img = ocr_agent::rgba_to_rgb(&full_rgba);
        let (lx, ly, lw, _lh) = fg.geometry()?;
        let (_sw, sh) = fg.screen_logical_size()?;
        let scale_w = full_img.width() as f32 / _sw.max(1) as f32;
        let scale_h = full_img.height() as f32 / sh.max(1) as f32;
        let scale = (scale_w + scale_h) / 2.0;
        let gx = (lx as f32 * scale) as i32;
        let gy = (ly as f32 * scale) as i32;
        let widgets = analyzer.analyze(&win_img)?;
        let reload = widgets
            .iter()
            .find(|w| w.label == live_label)
            .context(format!("窗口流里没找到 {} 控件", live_label))?;
        let (rx, ry, rww, rhh) = reload.rect;
        let ax = gx + rx as i32 + rww as i32 / 2;
        let ay = gy + ry as i32 + rhh as i32 / 2;
        eprintln!(
            "[move-only] {} 绝对坐标(物理)≈({}, {}) [offset=({},{}), scale={:.3}, 窗口内中心=({},{}), OCR识别{}个控件]",
            live_label,
            ax,
            ay,
            gx,
            gy,
            scale,
            rx + rww / 2,
            ry + rhh / 2,
            widgets.len()
        );
        // ax, ay 是物理坐标，转成逻辑再交给 ensure_move_to（其入口收逻辑坐标）。
        let (lax, lay) = (
            (ax as f32 / scale).round() as i32,
            (ay as f32 / scale).round() as i32,
        );
        let la_pos = IVec2::new(lax, lay);
        let op = ScreenOperator::new().with_foregrounder(fg.clone());
        op.ensure_move_to(la_pos).context("移动失败")?;
        let fg_read = ocr_agent::KdeForegrounder::new("testing_08");
        if let Ok(p_pos) = fg_read.cursor_pos() {
            eprintln!(
                "[move-only] 移动后 KWin 读到的逻辑光标=({p_pos}), 目标逻辑=({lax}, {lay}), 偏差=({}, {})",
                p_pos.x - lax,
                p_pos.y - lay
            );
        }
        eprintln!(
            "[move-only] 已移动（未点击）；请肉眼确认光标是否在 {} 上",
            live_label
        );
        return Ok(());
    }

    // ---- 自动闭环（live，ScreenCast 窗口流 + KWin 几何）----
    // 「看」用 portal **窗口流**抓 testing_08 自身合成表面（遮挡无关、不含 portal 浮
    // 层，且本机窗口捕获已预授权不弹阻塞对话框）；OCR 得到「窗口相对坐标」。「操作」
    // 用 YdotoolExecutor，其 offset = 窗口在屏幕上的位置，由 KdeForegrounder::geometry()
    // 经 KWin 脚本拿到（本机 Stream::position() 返回 None，故走 KWin）。点击前 raise
    // 目标到最前（Wayland 输入要求点击点最上层）。
    //
    // 注意：本机若有两个同名 testing_08 进程，portal 选窗可能不确定——请确保只运行
    // 你要验证的那一个（关掉另一个），否则可能识别/点到另一个实例。
    if live {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let analyzer = LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
            .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;

        // 前台器：点击前 raise 目标到最前。
        let fg = ocr_agent::KdeForegrounder::new("testing_08");
        fg.raise().context("把 testing_08 提到最前失败")?;

        // 构造捕获器：全屏 / 窗口分别用各自持久化的 token（两类不能串用）。
        let (saved_mon, saved_win) = load_tokens();
        let cap = match &saved_win {
            Some(t) => ScreenCastCapturer::with_window_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };
        let cap_full = match &saved_mon {
            Some(t) => ScreenCastCapturer::with_monitor_token(t.clone()),
            None => ScreenCastCapturer::new(),
        };

        // 「定位」用全屏 + 窗口流双图反推 offset：
        //   - 全屏（Monitor 源，屏幕绝对物理坐标空间）用来找窗口在屏幕上的位置；
        //   - 窗口流（Window 源，同一物理坐标空间，OCR 用）用来识别控件；
        //   - infer_window_offset(全屏, 窗口) 在全屏里模板匹配窗口，得到窗口左上角的
        //     屏幕绝对坐标（物理空间），与 OCR 坐标同空间，直接相加即可精确定位点击。
        // 这样既不混用 KWin 逻辑坐标（曾导致缩放错位），也不依赖 portal 的
        // Stream::position()（本机返回 None）。
        let full_rgba = async_io::block_on(cap_full.capture_fullscreen()).context(
            "抓全屏失败（首次可能需手动选屏授权；之后用 .cache/screencast_restore_token_monitor 免弹窗）",
        )?;
        let (win_rgba, _pos, new_token) = async_io::block_on(cap.capture_app_geom("")).context(
            "抓窗口流失败（首次可能需手动选窗授权；之后用 .cache/screencast_restore_token_window 免弹窗）",
        )?;
        if let Some(t) = &new_token {
            save_token("window", t)?;
            eprintln!("已保存窗口 token，下次运行不再弹选窗对话框");
        }
        let full_img = ocr_agent::rgba_to_rgb(&full_rgba);
        let win_img = ocr_agent::rgba_to_rgb(&win_rgba);

        // 「定位」用 KWin 几何（稳定真值）× 缩放比：
        //   - KWin 的 frameGeometry 给的是**逻辑像素**（含分数缩放，本机 160% → scale=1.6）；
        //   - scale 用**全屏 Monitor 流物理宽 / 屏幕逻辑宽**算（屏幕逻辑宽来自 KWin
        //     `workspace.displaySize`，即系统设置的缩放率直接体现），这是屏幕真实全局
        //     缩放，比「窗口流 buffer 宽 / 窗口逻辑宽」更稳（窗口 buffer 自带取整噪声）。
        //   - 物理 offset = 逻辑 x,y × scale。
        let (lx, ly, lw, _lh) = fg.geometry().context("取 KWin 窗口几何失败")?;
        let (_sw, sh) = fg.screen_logical_size().context("取屏幕逻辑尺寸失败")?;
        // 用全屏流物理尺寸除以屏幕逻辑尺寸得到全局 scale（优先宽，退化用高，取平均更稳）。
        let scale_w = full_img.width() as f32 / _sw.max(1) as f32;
        let scale_h = full_img.height() as f32 / sh.max(1) as f32;
        let scale = (scale_w + scale_h) / 2.0;
        let gx = (lx as f32 * scale) as i32;
        let gy = (ly as f32 * scale) as i32;
        eprintln!(
            "窗口屏幕位置(物理) offset=(x={}, y={}) [KWin 逻辑 ({},{}), scale={:.3} (全屏{}x{} / 逻辑{}x{}), 窗口流 {}x{}]",
            gx,
            gy,
            lx,
            ly,
            scale,
            full_img.width(),
            full_img.height(),
            _sw,
            sh,
            win_img.width(),
            win_img.height()
        );
        // debug 交叉验证：图像滑窗反推（仅供对比，不影响点击）。
        let (ix, iy) = ocr_agent::infer_window_offset(&full_img, &win_img);
        eprintln!(
            "  [交叉验证] infer_window_offset=(x={}, y={})；与 KWin 差 ({}, {}){}",
            ix,
            iy,
            gx - ix,
            gy - iy,
            if (gx - ix).abs() > 40 || (gy - iy).abs() > 40 {
                "  ⚠ 偏差较大，图像反推可能误匹配"
            } else {
                ""
            }
        );

        // 执行器：real 时用相对移动模式（绕开失效的 ydotool 绝对移动）真点；否则 PrintExecutor。
        let executor: Box<dyn Executor> = if real {
            eprintln!(
                "使用 YdotoolExecutor（相对移动模式，窗口相对→绝对坐标点击）真点 {}",
                live_label
            );
            // 用 KdeForegrounder 提供闭环读数（cursor_pos），配合相对移动模拟绝对定位。
            Box::new(YdotoolExecutor::with_foregrounder(
                (gx, gy),
                scale,
                fg.clone(),
            ))
        } else {
            eprintln!(
                "使用 PrintExecutor（dry-run，不真点）；加 --real 才会真的点 {}",
                live_label
            );
            Box::new(PrintExecutor)
        };
        let mut agent = Agent::with_foregrounder(analyzer, executor, Box::new(fg.clone()));

        // 调试出图：开启后每帧把「窗口 OCR 标注 + 全屏红框(app)/绿十字(落点)」存到 tmp/。
        if debug {
            agent.set_debug(Some(repo_root().join("tmp")));
        }

        // 闭环：窗口流抓「点击前/后」两帧（ocr 坐标相对窗口），点击换算绝对坐标。
        // 传入 cap_full 让调试模式能把定位画到全屏上（正常运行可传 None，这里直接传）。
        let result = async_io::block_on(agent.verify_click_stream(
            &cap,
            Some(&cap_full),
            &live_label,
            Some((gx, gy)),
        ))?;

        println!(
            "闭环(live,窗口流)：before={:?} after={:?} label={:?} delta={:?}",
            result.before,
            result.after,
            result.label,
            result.delta()
        );
        if result.delta() == Some(50) {
            println!("✓ delta=50，与 testing_08 的 Reload(count+=50) 一致，自动闭环通过");
        } else {
            println!(
                "（delta 非 50：识别偏差 / 点击未生效 / 该按钮语义非 +50；dry-run 下 delta 必然为 0）"
            );
        }
        return Ok(());
    }

    // ---- 闭环验证模式：直接对比两帧计数 ----
    if let (Some(b), Some(a)) = (verify_before, verify_after) {
        let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
        let analyzer = LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
            .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;
        let mut agent = Agent::new(analyzer, Box::new(PrintExecutor));

        let img_b = image::open(&b)
            .with_context(|| format!("读取 before 图失败: {b}"))?
            .to_rgb8();
        let img_a = image::open(&a)
            .with_context(|| format!("读取 after 图失败: {a}"))?
            .to_rgb8();
        let before = agent.read_count(&img_b)?;
        let after = agent.read_count(&img_a)?;
        let delta = match (before, after) {
            (Some(b), Some(a)) => Some(a - b),
            _ => None,
        };
        println!(
            "闭环验证：before={:?} after={:?} delta={:?}",
            before, after, delta
        );
        if delta == Some(50) {
            println!("✓ delta=50，与 testing_08 的 Reload(count+=50) 一致，闭环通过");
        } else {
            println!("（delta 非 50：可能 before/after 不是 Reload 点击前后，或识别有偏差）");
        }
        return Ok(());
    }

    let image_path = args[1].clone();
    let img = image::open(&image_path)
        .with_context(|| format!("读取图片失败: {image_path}"))?
        .to_rgb8();

    // ---- 自动反推窗口偏移（离线双图：全屏 + 窗口流）----
    let inferred: Option<(i32, i32)> = if auto_offset {
        let (f, w) = match (full_path.clone(), window_path.clone()) {
            (Some(f), Some(w)) => (f, w),
            _ => anyhow::bail!("--auto-offset 需要 --full <全屏图> --window <窗口图>"),
        };
        let full = image::open(&f)
            .with_context(|| format!("读取全屏图失败: {f}"))?
            .to_rgb8();
        let win = image::open(&w)
            .with_context(|| format!("读取窗口图失败: {w}"))?
            .to_rgb8();
        let off = infer_window_offset(&full, &win);
        eprintln!("自动反推偏移（双图）= {:?}", off);
        Some(off)
    } else {
        None
    };

    let model_dir = repo_root().join(rapidocr_ort::DEFAULT_MODEL_DIR);
    let analyzer = LayoutAnalyzer::with_ocr(ModelProfile::V3, &model_dir, Default::default())
        .context("构建 OCR 引擎失败（确认 data/models/rapidocr 权重就绪）")?;

    // 选择执行器：--real 时用推断（或手动）偏移，否则 PrintExecutor。
    let executor: Box<dyn Executor> = if real {
        let (ox, oy) = inferred.unwrap_or_else(|| {
            eprintln!("警告：--real 但无 --auto-offset，偏移按 (0,0)，点击可能偏移到屏幕 (0,0)");
            (0, 0)
        });
        eprintln!("使用 YdotoolExecutor（窗口偏移 {},{}）", ox, oy);
        Box::new(YdotoolExecutor::new((ox, oy)))
    } else {
        eprintln!(
            "使用 PrintExecutor（dry-run，不真点）{}",
            inferred
                .map(|o| format!("；推断偏移 {:?}", o))
                .unwrap_or_default()
        );
        Box::new(PrintExecutor)
    };

    let mut agent = Agent::new(analyzer, executor);

    // 1. 读当前计数。
    let before = agent.read_count(&img)?;
    println!("点击前 count = {:?}", before);

    // 2. 按标签点击（演示 Reload / Load）。
    for label in ["Reload", "Load"] {
        if let Err(e) = agent.click_by_label(&img, label) {
            eprintln!("点击 {} 失败: {:#}", label, e);
        }
    }

    // 3. 再读一次（PrintExecutor 未真点，值应不变；YdotoolExecutor 会变化）。
    let after = agent.read_count(&img)?;
    println!("点击后 count = {:?}", after);

    Ok(())
}
