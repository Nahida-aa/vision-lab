//! 用 OpenCV VideoCapture 解码提取第 N 帧的 BGR，写 raw 文件。
//! 用于 A/B 对比：提取与状态机一致的帧喂 ab_dump。
use std::env;

fn main() {
    let a: Vec<String> = env::args().collect();
    if a.len() < 4 {
        eprintln!("用法: {} <video> <frame_idx> <out.raw>", a[0]);
        std::process::exit(1);
    }
    let path = &a[1];
    let target: i64 = a[2].parse().unwrap();
    let out = &a[3];

    use opencv::prelude::*;
    let mut cap = opencv::videoio::VideoCapture::from_file(path, opencv::videoio::CAP_ANY)
        .expect("打开失败");
    for _ in 0..target {
        let mut f = opencv::core::Mat::default();
        if !cap.read(&mut f).expect("read 失败") {
            eprintln!("帧不足 {}", target);
            std::process::exit(1);
        }
    }
    let mut f = opencv::core::Mat::default();
    cap.read(&mut f).expect("read 失败");
    let data = f.data_bytes().expect("取数据失败");
    std::fs::write(out, data).expect("写文件失败");
    println!("帧 {} 尺寸 {}x{} 已写 {}", target, f.cols(), f.rows(), out);
}
