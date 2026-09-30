//! geometry 内部 sobel_m_edge 性能基准（release，同 crate 内联）。
//!
//! 目的：测 `sobel_m_edge` **在 geometry crate 内**的速度。因为 `#[target_feature]`
//! 函数不能跨 crate 内联（Rust issue #145574），跨 crate（subtitle_finder 调 geometry）
//! 会丢失内联、比 crate 内慢 ~1.8×。本 bin 在 geometry 内部调用，能反映"内联后"的
//! 真实最快速度，用于对比跨 crate 的 perfbench 数字。
//!
//! 跑法：`cargo run -p geometry --release --bin sobel_bench`
//! （注意：cargo test 用 dev profile 无优化，测出的数字不可信，须 --release。）

use std::time::Instant;

use geometry::imgproc::{sobel_h_edge, sobel_m_edge, sobel_n_edge};
const W: usize = 1280;
const H: usize = 720;

/// 确定性伪随机灰度图 + 一条垂直边缘（模拟真实图像）。
fn make_gray() -> Vec<u8> {
    let mut y = vec![0u8; W * H];
    let mut seed = 0x12345678u32;
    for i in 0..W * H {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        y[i] = (seed >> 24) as u8;
    }
    for yy in 0..H {
        for x in (W / 2)..W {
            y[yy * W + x] = y[yy * W + x].wrapping_add(80);
        }
    }
    y
}

fn bench<T>(name: &str, iters: usize, mut f: impl FnMut() -> T) {
    for _ in 0..5 {
        std::hint::black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(f());
    }
    let per = start.elapsed().as_secs_f64() / iters as f64;
    println!(
        "{:<32} {:>6} 次  {:>8.3} ms/次  ({:>8.1} ns/次)",
        name,
        iters,
        per * 1e3,
        per * 1e9
    );
}

fn main() {
    let src = make_gray();
    println!("=== geometry 内部 sobel（720p，release，同 crate 内联）===");
    bench("sobel_m_edge (crate内)", 2000, || sobel_m_edge(&src, W, H));
    bench("sobel_n_edge (crate内)", 2000, || sobel_n_edge(&src, W, H));
    bench("sobel_h_edge (crate内)", 2000, || sobel_h_edge(&src, W, H));

    // _into 版（复用输出缓冲，不含 0.92MB u16 分配）。
    let mut out = vec![0u16; W * H];
    let t0 = Instant::now();
    for _ in 0..2000 {
        geometry::imgproc::sobel_m_edge_into(&src, W, H, &mut out);
    }
    let per = t0.elapsed().as_secs_f64() / 2000.0;
    println!("sobel_m_edge_into (crate内,无分配)  {:>8.3} ms/次", per * 1e3);

    // aply_ess / aply_ecp（im_ff 里 thr1/thr2 各调一次，是 im_ff 最大头 43%）。
    let u16_src = make_u16();
    println!("\n=== geometry 内部 aply_ess / aply_ecp（720p，release，crate 内）===");
    bench("aply_ess (crate内)", 2000, || {
        geometry::imgproc::aply_ess(&u16_src, W, H)
    });
    bench("aply_ecp (crate内)", 2000, || {
        geometry::imgproc::aply_ecp(&u16_src, W, H)
    });
}

/// 合成 u16 边缘图（有大量非 0，模拟 CMOE 输出，让 ess/ecp 全核跑）。
fn make_u16() -> Vec<u16> {
    let mut im = vec![0u16; W * H];
    let mut seed = 42u32;
    for i in 0..W * H {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        im[i] = ((seed >> 16) % 600) as u16; // 0..600 边缘强度
    }
    im
}
