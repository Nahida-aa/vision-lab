//! subtitle_finder CLI：对视频跑 FastSearchSubtitles，输出字幕关键帧 + 时间轴。
//!
//! 用法：
//!   cargo run -p subtitle_finder --release -- <video.mp4>
//!   cargo run -p subtitle_finder --release -- --profile <video.mp4>  # 附性能剖析
//!   cargo run -p subtitle_finder --release -- --output timeline <video.mp4>  # 只写 timeline.txt
//!   cargo run -p subtitle_finder --release -- --output frames <video.mp4>    # 仅原始关键帧 PNG
//!
//! 落盘由 `output::write_artifacts` 完成（库方式也可直接用），产物均落在
//! `--out` 目录下：`frames/`（原始关键帧 PNG）、`mask/`（去背景掩码）、
//! `timeline.txt`、`keyframes.json`。详见 `subtitle_finder::output`。
//!
//! CLI 只负责参数解析 / 路径解析 / 进度条 UX；核心逻辑（解码 + 状态机 +
//! 落盘）都在库 `subtitle_finder` 里，便于以库方式复用。

use anyhow::Context;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};

// 高并发 malloc：多 worker transform + 每帧 1MB 级分配，system malloc 锁竞争明显。
// A/B 时注释掉下面两行即可回落到 glibc allocator。
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use subtitle_finder::output::{self, OutputMode};

#[derive(Parser, Debug)]
#[command(name = "subtitle_finder", about = "对视频跑字幕关键帧筛选，输出关键帧 + 时间轴")]
struct Cli {
    /// 附性能剖析（分阶段耗时统计）。
    #[arg(long)]
    profile: bool,

    /// 输出模式：full=全量产物，timeline=只写时间轴，frames=仅原始关键帧 PNG。
    #[arg(long, value_enum, default_value_t = OutputMode::Full)]
    output: OutputMode,

    /// 输出目录（相对仓库根；默认包内 out/）。
    #[arg(long)]
    out: Option<String>,

    /// 输入视频路径（相对仓库根，或绝对路径）。
    #[arg(required = true)]
    video: String,
}

/// 仓库根：二进制在 `target/release/subtitle_finder`，上溯两级到 workspace 根。
fn repo_root() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前可执行文件路径失败")?;
    let exe_dir = exe.parent().context("可执行文件无父目录")?.to_path_buf();
    let root = exe_dir
        .join("..") // 去掉 target/release
        .join("..") // 去掉 packages/subtitle_finder
        .canonicalize()
        .context("解析仓库根失败（确认从仓库内构建）")?;
    Ok(root)
}

/// 相对路径相对仓库根解析；绝对路径原样返回。
fn resolve_path(root: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn main() -> anyhow::Result<()> {
    // 初始化结构化日志（`RUST_LOG=subtitle_finder=debug` 可开调试日志）。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let profile = cli.profile;
    let mode = cli.output;

    // 相对路径相对仓库根解析（与 subtitle_ocr 一致：二进制在 target/ 下，
    // 上溯两级即仓库根），不依赖调用方当前工作目录。
    let repo_root = repo_root()?;
    let video = resolve_path(&repo_root, &cli.video);
    if !video.exists() {
        anyhow::bail!("视频不存在: {}", video.display());
    }

    let params = subtitle_finder::params::Params::default();
    // 用带剖析的帧缓存。
    let mut cache = subtitle_finder::state::FrameCache::new(&video, &params);
    if profile {
        cache = cache.with_profiling();
    }
    // 强制打开解码器（惰性），使 total_frames() 在进度条建条前已就绪。
    cache.advance_to(0)?;

    // 解码阶段进度条（按帧数，走 stderr；不依赖 duration，始终有总量）。
    // total==0（nb_frames 与 duration×fps 皆不可得）时退化为无总量进度。
    let total_frames = cache.total_frames().max(0) as u64;
    let pb = ProgressBar::new(total_frames);
    if total_frames == 0 {
        // 未知总帧数：隐藏长度，仅显示已解码帧数 + 耗时。
        pb.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] 解码 {pos} 帧")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
    } else {
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] 解码 [{bar:30.cyan/blue}] {pos}/{len} 帧 ({eta})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
        );
    }
    let kfs = subtitle_finder::state::find_keyframes_with_cache_progress(
        &mut cache,
        &params,
        &mut |decoded, _total| {
            pb.set_position(decoded);
        },
    )?;
    // 保留进度条满格那一行（不消失，对齐 subtitle_ocr 的 finish 行为）；
    // 耗时用 eprintln 另起一行（stderr 直写一定落盘，便于事后回看总耗时）。
    let decode_elapsed = pb.elapsed();
    pb.finish();
    eprintln!("解码完成 {} 帧，耗时 {:.1}s", cache.len(), decode_elapsed.as_secs_f64());

    if profile {
        let n = cache.len();
        if let Some(pf) = cache.profiler() {
            pf.dump(n);
            eprintln!("主线程等待流水线: {:.0} ms", cache.stream_wait_ms());
        }
    }

    println!("找到 {} 个关键帧", kfs.len());

    // 输出目录：默认包内 out/（相对 manifest 目录）；--out 指定则相对仓库根解析。
    let out_dir = match &cli.out {
        Some(d) => resolve_path(&repo_root, d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out"),
    };

    // 落盘编排在库 output::write_artifacts；进度回调驱动落盘进度条。
    let save_pb = ProgressBar::new(kfs.len() as u64);
    save_pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] 落盘 [{bar:30.green}] {pos}/{len}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );
    let report = output::write_artifacts(&kfs, &out_dir, mode, &mut |written| {
        save_pb.set_position(written as u64);
    })?;
    let save_elapsed = save_pb.elapsed();
    save_pb.finish();
    eprintln!(
        "落盘完成 {} 张，耗时 {:.1}s",
        report.frames_written,
        save_elapsed.as_secs_f64()
    );

    println!("输出目录: {}", out_dir.display());
    Ok(())
}
