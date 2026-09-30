//! A/B 验证工具：跑 `get_transformed_image` 各阶段，dump 指纹（区域白点+校验和）+ 原始数组。
//!
//! 用法：`cargo run -p subtitle_finder --release --bin ab_dump -- <bgr.raw> <w> <h> <out_prefix>`
//!
//! 与 C++ `cli/ab_dump.cpp` 输出相同格式指纹，供 `scripts/ab_compare.py` 逐阶段对比，
//! 二分定位 Rust/C++ 首个像素级差异（幽灵带 bug 诊断）。

use std::env;
use std::io::Write;

use subtitle_finder::filter;
use subtitle_finder::imgops;
use subtitle_finder::params::Params;

/// 行区域白点计数 + 全图白点 + 校验和（简版 FNV-1a，避免额外依赖）。
fn fingerprint(im: &[u8], w: usize) -> (usize, usize, usize, u32) {
    let total = im.iter().filter(|&&v| v == 255).count();
    let top26 = im[26 * w..45 * w].iter().filter(|&&v| v == 255).count();
    let mid331 = im[331 * w..340 * w].iter().filter(|&&v| v == 255).count();
    // FNV-1a 64 → 低 32。
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in im {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (total, top26, mid331, (h & 0xffff_ffff) as u32)
}

fn dump_stage(prefix: &str, name: &str, im: &[u8], w: usize) {
    let (total, top, mid, hash) = fingerprint(im, w);
    println!("stage={} total={} top26={} mid331={} hash={:#x}", name, total, top, mid, hash);
    let path = format!("{}.{}.raw", prefix, name);
    let mut f = std::fs::File::create(&path).expect("写文件失败");
    f.write_all(im).expect("写文件失败");
}

fn main() {
    tracing_subscriber::fmt::init();
    let a: Vec<String> = env::args().collect();
    if a.len() < 5 {
        eprintln!("用法: {} <bgr.raw> <w> <h> <out_prefix>", a[0]);
        std::process::exit(1);
    }
    let bgr = std::fs::read(&a[1]).expect("读 BGR 失败");
    let w: usize = a[2].parse().unwrap();
    let h: usize = a[3].parse().unwrap();
    let prefix = &a[4];
    let p = Params::default();

    // 1) color_filtration → bands。
    let (lb, le, n) = imgops::color_filtration(&bgr, w, h, &p);
    println!("stage=color_filtration n={} lb={:?} le={:?}", n, lb, le);
    if n == 0 {
        println!("color_filtration 无带，退出");
        std::process::exit(0);
    }

    // 2) bgr_to_yuv。
    let (mut im_y, mut im_u, mut im_v) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
    imgops::bgr_to_yuv(&bgr, &mut im_y, &mut im_u, &mut im_v, w, h);
    dump_stage(prefix, "y", &im_y, w);
    dump_stage(prefix, "u", &im_u, w);
    dump_stage(prefix, "v", &im_v, w);

    // 3) get_im_ff / get_im_ne / get_im_he。
    let (im_ff, im_sf, lb_a, le_a) =
        imgops::get_im_ff(&im_y, &im_u, &im_v, &lb, &le, n, w, h, &p, None);
    let im_ne0 = imgops::get_im_ne(&im_y, &im_u, &im_v, w, h, &p);
    let im_he = imgops::get_im_he(&im_y, &im_u, &im_v, w, h, &p);
    dump_stage(prefix, "ff", &im_ff, w);
    dump_stage(prefix, "sf", &im_sf, w);
    dump_stage(prefix, "ne0", &im_ne0, w);
    dump_stage(prefix, "he", &im_he, w);
    println!("stage=get_im_ff lb_a={:?} le_a={:?}", lb_a, le_a);

    // 4) NE ∪ HE。
    let mut im_ne = im_ne0;
    imgops::combine_two_images(&mut im_ne, &im_he, 255);
    dump_stage(prefix, "ne", &im_ne, w);

    // 5) filter_transformed_image → TF/has_text。
    // 先复现 step1: im_sf ∩ dilate(NE)（filter_transformed_image 第一步）。
    let ne_dil = imgops::dilate(&im_ne, w, h, ((p.min_h * h as f32) as i32) / 2);
    let mut sf_step1 = im_sf.clone();
    imgops::intersect_two_images_inplace(&mut sf_step1, &ne_dil, 0u8);
    dump_stage(prefix, "sf_step1", &sf_step1, w);
    let mut im_tf = vec![0u8; w * h];
    let mut im_sf_mut = im_sf.clone();
    let has_text = filter::filter_transformed_image(
        &im_ff, &mut im_sf_mut, &mut im_tf, &im_ne, &lb_a, &le_a, n, w, h, &p,
    );
    dump_stage(prefix, "tf", &im_tf, w);
    dump_stage(prefix, "sf_filtered", &im_sf_mut, w);
    println!("stage=filter_transformed_image has_text={}", has_text);
}
