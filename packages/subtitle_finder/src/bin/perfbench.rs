//! 状态机关键函数微基准：隔离测 `compare_two_subs_optimal` / imgops 原语的真实开销。
//!
//! 目的：在整视频 pipeline 之外，单独测某个函数的每调用耗时，判断「分配优化」
//! 是否真的有墙钟收益。跑法：`cargo run -p subtitle_finder --release --bin perfbench`。
//!
//! 数据用 1280×720 合成字幕帧（几行白色文字），真实像素量级，函数语义与真帧一致。

use std::time::Instant;

use subtitle_finder::compare::compare_two_subs_optimal;
use subtitle_finder::imgops;
use subtitle_finder::imgops::Profiler;
use subtitle_finder::params::Params;

const W: usize = 1280;
const H: usize = 720;

/// 合成一帧：底部有 1-2 行白字（255），其余 0。模拟 ISA 前景图。
fn make_text_frame(line1: bool, line2: bool) -> Vec<u8> {
    let mut im = vec![0u8; W * H];
    if line1 {
        for y in 20..40 {
            for x in 200..1080 {
                im[y * W + x] = 255;
            }
        }
    }
    if line2 {
        for y in 45..60 {
            for x in 150..1100 {
                im[y * W + x] = 255;
            }
        }
    }
    im
}

fn bench<T>(name: &str, iters: usize, mut f: impl FnMut() -> T) {
    // 预热。
    for _ in 0..10 {
        std::hint::black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(f());
    }
    let dur = start.elapsed();
    let per = dur.as_secs_f64() / iters as f64;
    println!(
        "{:<40} {:>6} 次  {:>8.3} ms/次   ({:>8.2} ns/次, 总 {:>7.3}s)",
        name,
        iters,
        per * 1e3,
        per * 1e9,
        dur.as_secs_f64()
    );
}

fn main() {
    let p = Params::default();

    // 两帧：行1一致、行2有差异 → compare 会走完整路径（首轮或 DifficultCompare）。
    let im_a = make_text_frame(true, true);
    let im_b = make_text_frame(true, false);
    let ve_a = imgops::dilate(&im_a, W, H, 1);
    let ve_b = imgops::dilate(&im_b, W, H, 1);
    // 相同帧 → 返回 true（无变化），走 compare_two_subs 首轮直接命中。
    let im_same = make_text_frame(true, true);
    let ve_same = imgops::dilate(&im_same, W, H, 1);

    println!("=== compare_two_subs_optimal（1280x720）===");

    // 1) 相同帧：最热路径（追踪阶段 bln==true 时每帧都调）。
    let (a, b, va, vb) = (&im_a, &im_b, &ve_a, &ve_b);
    bench("compare_optimal 相同帧(无变化)", 2000, || {
        compare_two_subs_optimal(a, None, va, None, a, None, va, W, H, 0, W as i32 - 1, &p)
    });
    // 2) 差异帧：可能触发 DifficultCompare 的昂贵路径。
    bench("compare_optimal 差异帧", 2000, || {
        compare_two_subs_optimal(a, None, va, None, b, None, vb, W, H, 0, W as i32 - 1, &p)
    });

    println!("\n=== imgops 原语（单次整帧分配）===");
    bench("add_two_images", 5000, || {
        imgops::add_two_images(a, b, W * H)
    });
    bench("dilate(iters=1)", 2000, || {
        imgops::dilate(a, W, H, 1)
    });
    // filter 里真实用法是 dilate(iters=6)（NE 边缘图）。用较密的边缘图测。
    let dense_edges = make_edge_dense();
    bench("dilate(iters=6) 边缘图", 200, || {
        imgops::dilate(&dense_edges, W, H, 6)
    });
    bench("intersect_two_images_inplace", 5000, || {
        let mut x = a.clone();
        imgops::intersect_two_images_inplace(&mut x, b, 0u8);
        x
    });

    // 参照：compare 相对 20ms/帧 变换的占比。
    println!(
        "\n参照：整视频每帧约 20ms（其中 filter 9ms + im_ff 7ms）。上面的 compare 每次 {}",
        "若远小于 1ms 则状态机分配非瓶颈"
    );
    let _ = im_same;
    let _ = ve_same;

    println!("\n=== get_transformed_image（每帧变换，1280x720 BGR）===");
    // 合成一帧带文字的 BGR（白色文字 / 彩色背景），喂完整变换管线。
    let bgr = make_bgr_frame();
    let (w, h) = (W, H);

    // geometry Sobel（跨 crate 调用，subtitle_finder im_ff 用它）单通道 720p。
    let gray = make_gray_frame();
    bench("geometry sobel_m_edge (跨crate)", 2000, || {
        geometry::imgproc::sobel_m_edge(&gray, W, H)
    });
    bench("geometry sobel_n_edge (跨crate)", 2000, || {
        geometry::imgproc::sobel_n_edge(&gray, W, H)
    });
    bench("geometry sobel_h_edge (跨crate)", 2000, || {
        geometry::imgproc::sobel_h_edge(&gray, W, H)
    });
    let _ = (w, h);

    // 端到端每帧耗时（不含解码）。
    bench("get_transformed_image 端到端", 200, || {
        imgops::get_transformed_image(&bgr, w, h, &p, None)
    });

    // 分阶段拆解：用 Profiler 累计每阶段耗时，除以次数得每帧每阶段。
    println!("\n  -- get_transformed_image 分阶段（Profiler 累计 / 次数）--");
    let mut prof = Profiler::new();
    prof.enable();
    let iters = 200usize;
    for _ in 0..iters {
        imgops::get_transformed_image(&bgr, w, h, &p, Some(&mut prof));
    }
    println!(
        "{:<30} {:>8.3} ms/帧",
        "color_filtration",
        prof.color_filtration_ms / iters as f64
    );
    println!(
        "{:<30} {:>8.3} ms/帧",
        "bgr_to_yuv",
        prof.bgr_to_yuv_ms / iters as f64
    );
    println!("{:<30} {:>8.3} ms/帧", "im_ff(边缘,3线程)", prof.im_ff_ms / iters as f64);
    println!("{:<30} {:>8.3} ms/帧", "filter(连通域)", prof.filter_ms / iters as f64);
    println!(
        "{:<30} {:>8.3} ms/帧",
        "合计",
        prof.total_ms() / iters as f64
    );
}

/// 合成一帧类边缘图：稀疏横/竖线 + 少量噪声（模拟 Sobel 边缘 NE 图）。
/// ⚠️ dilate 是 scatter（只在白点处工作），耗时随**白点密度**线性增长。
/// 真实 NE 边缘稀疏，这里保持稀疏才贴近实际（密集合成数据会高估 dilate 3-4×）。
fn make_edge_dense() -> Vec<u8> {
    let mut im = vec![0u8; W * H];
    // 稀疏横线（每 9 行一条）、稀疏竖线（每 12 列一条）。
    for y in (0..H).step_by(9) {
        for x in 0..W {
            im[y * W + x] = 255;
        }
    }
    for x in (0..W).step_by(12) {
        for y in 0..H {
            im[y * W + x] = 255;
        }
    }
    // 少量噪声点（~1.5% 密度）。
    let mut seed = 777u32;
    for i in 0..W * H {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        if (seed >> 16) % 70 == 0 {
            im[i] = 255;
        }
    }
    im
}

/// 合成一帧 1280x720 灰度图（模拟 Y 通道）。
fn make_gray_frame() -> Vec<u8> {
    let mut y = vec![0u8; W * H];
    let mut seed = 999u32;
    for i in 0..W * H {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        y[i] = (seed >> 24) as u8;
    }
    // 叠加一条垂直边缘。
    for yy in 0..H {
        for x in (W / 2)..W {
            y[yy * W + x] = y[yy * W + x].wrapping_add(80);
        }
    }
    y
}

/// 合成一帧 1280x720 BGR：浅灰背景 + 底部几行**带边缘变奏**的白色文字。
/// 通道顺序 BGR。文字内每像素加少量强度扰动（模拟抗锯齿/描边），使
/// `color_filtration` 的每 8px 段色差能超过 `scd` 触发文字带检测（否则 n==0 提前返回）。
fn make_bgr_frame() -> Vec<u8> {
    let mut bgr = vec![0u8; W * H * 3];
    // 背景：浅灰 (200,200,200)。
    for i in 0..W * H {
        let base = i * 3;
        bgr[base] = 200;
        bgr[base + 1] = 200;
        bgr[base + 2] = 200;
    }
    // 三行文字，带确定性强度扰动（伪随机但稳定）。
    let rows: &[(usize, usize)] = &[(50, 80), (150, 180), (600, 630)];
    let mut seed = 12345u32;
    for &(r0, r1) in rows {
        for y in r0..r1 {
            for x in 100..1200 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let wobble = (seed >> 24) as u8; // 0..255
                let base = (y * W + x) * 3;
                bgr[base] = 220 + wobble / 4;
                bgr[base + 1] = 220 + wobble / 4;
                bgr[base + 2] = 220 + wobble / 4;
            }
        }
    }
    bgr
}
