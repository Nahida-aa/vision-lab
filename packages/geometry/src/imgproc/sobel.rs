//! Sobel 边缘检测：M / N / H 三种（复刻 VideoSubFinder 的 ImprovedSobelMEdge /
//! FastImprovedSobelNEdge / FastImprovedSobelHEdge）。
//!
//! 单通道 u8 灰度 → u16 边缘强度，边界像素为 0。
//! - AVX2 路径：16×i16（`load16_i16`，一次 16 像素）。
//! - AVX-512 路径（sobel_n/h）：一次 32 像素（zmm），并用 **3-load + permutexvar_epi16**
//!   派生 ±1 偏移 tap，减少内存带宽（带宽 bound，实测 6-load→3-load 快 ~2×）。
//!
//! # 性能与 Rust 平台限制（重要，改这里前必读）
//!
//! 对照基准 `tools/perf_compare/`：g++ `-O3 -march=native` 用 AVX-512（zmm，一次 32 像素）
//! 在调用点自动向量化，sobel_n/h ~0.10ms。我们的同 crate 内联版也能到 ~0.11ms，
//! **但跨 crate（subtitle_finder 调 geometry）受 Rust 限制只能 ~0.18ms**。原因：
//!
//! 1. `#[inline(always)]` 不能与 `#[target_feature]` 同用（Rust issue #145574，
//!    nightly 同样禁止，是设计限制）。
//! 2. 运行时 `is_x86_feature_detected` 分派让 LLVM 无法静态确定调用方需要 avx512，
//!    故即使开 thin LTO + codegen-units=1，跨 crate 也不内联 `#[target_feature]` 函数。
//! 3. 唯一让调用方静态确定 feature 的办法是把分派函数也标 `#[target_feature]`，但那会让
//!    无 avx512 机器的 fallback 失效。
//!
//! 所以 sobel_n/h 跨 crate 仍 ~1.8× 慢于 C++，这是 Rust 平台约束，非算法问题。
//! 若未来想彻底解决：把 Sobel 内联进调用方（subtitle_finder），或等 Rust 支持
//! `#[target_feature]` 跨 crate 内联。

// ============================================================================
// Sobel 边缘检测（复刻 VideoSubFinder 的 ImprovedSobelMEdge / FastImprovedSobelNEdge /
// FastImprovedSobelHEdge）。单通道 u8 灰度 → u16 边缘强度。边界像素为 0。
// ============================================================================

// AVX-512 的 permutexvar_epi16 索引（i16 lane）。用 static 常量 + load（而非每次
// `_mm512_set_epi16` 运行时构建），因为 `#[target_feature]` 函数不被内联，若在函数内
// 用 set_epi16 会每次调用都重建向量，开销巨大。
//
// shift_right1（lane k = v[k-1]）：idx[0]=0 占位（用 mask 替换），idx[1..]=0..30。
static SR_IDX: [i16; 32] = [
    0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
    22, 23, 24, 25, 26, 27, 28, 29, 30,
];
// shift_left1（lane k = v[k+1]）：idx[31]=31 占位（用 mask 替换），idx[0..]=1..31。
static SL_IDX: [i16; 32] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
    24, 25, 26, 27, 28, 29, 30, 31, 31,
];

/// ImprovedSobelMEdge：4 方向梯度取 max（3x3 stencil）。
pub fn sobel_m_edge(src: &[u8], w: usize, h: usize) -> Vec<u16> {
    let mut out = vec![0u16; w * h];
    sobel_m_edge_into(src, w, h, &mut out);
    out
}

/// 写进调用方提供的 buffer（避免每次分配，供基准测纯计算）。
pub fn sobel_m_edge_into(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    debug_assert!(out.len() >= w * h);
    if std::env::var("SF_FORCE_SCALAR_SOBEL").is_ok() {
        sobel_m_edge_scalar(src, w, h, out);
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        unsafe { sobel_m_edge_avx2(src, w, h, out) }
        return;
    }
    sobel_m_edge_scalar(src, w, h, out);
}

/// FastImprovedSobelNEdge：垂直边缘（3 行 stencil）。
pub fn sobel_n_edge(src: &[u8], w: usize, h: usize) -> Vec<u16> {
    let mut out = vec![0u16; w * h];
    sobel_n_edge_into(src, w, h, &mut out);
    out
}

/// 写进调用方提供的 buffer（避免每次分配，供基准测纯计算）。
pub fn sobel_n_edge_into(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    debug_assert!(out.len() >= w * h);
    #[cfg(target_arch = "x86_64")]
    {
        // AVX-512 优先（zmm 32 像素/次，追平 g++），否则 AVX2。
        if std::arch::is_x86_feature_detected!("avx512bw") && std::arch::is_x86_feature_detected!("avx512vl") {
            unsafe { sobel_n_edge_avx512(src, w, h, out) }
            return;
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            unsafe { sobel_n_edge_avx2(src, w, h, out) }
            return;
        }
    }
    sobel_n_edge_scalar(src, w, h, out);
}

/// FastImprovedSobelHEdge：水平边缘（3 行 stencil）。
pub fn sobel_h_edge(src: &[u8], w: usize, h: usize) -> Vec<u16> {
    let mut out = vec![0u16; w * h];
    sobel_h_edge_into(src, w, h, &mut out);
    out
}

/// 写进调用方提供的 buffer（避免每次分配，供基准测纯计算）。
pub fn sobel_h_edge_into(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    debug_assert!(out.len() >= w * h);
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512bw") && std::arch::is_x86_feature_detected!("avx512vl") {
            unsafe { sobel_h_edge_avx512(src, w, h, out) }
            return;
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            unsafe { sobel_h_edge_avx2(src, w, h, out) }
            return;
        }
    }
    sobel_h_edge_scalar(src, w, h, out);
}

fn sobel_m_edge_scalar(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let i = y * w + x;
            let lt = src[i - w - 1] as i32;
            let rt = src[i - w + 1] as i32;
            let mt = src[i - w] as i32;
            let lm = src[i - 1] as i32;
            let rm = src[i + 1] as i32;
            let mb = src[i + w] as i32;
            let lb = src[i + w - 1] as i32;
            let rb = src[i + w + 1] as i32;
            let val1 = lt - rb;
            let val2 = rt - lb;
            let val3 = mt - mb;
            let val4 = lm - rm;
            let mut max = (3 * (val1 + val2) + 10 * val3).abs();
            let mut v = (3 * (val1 - val2) + 10 * val4).abs();
            if max < v {
                max = v;
            }
            v = (3 * (val3 + val4) + 10 * val1).abs();
            if max < v {
                max = v;
            }
            v = (3 * (val3 - val4) + 10 * val2).abs();
            if max < v {
                max = v;
            }
            out[i] = max as u16;
        }
    }
}

fn sobel_n_edge_scalar(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let i = y * w + x;
            let up = src[i - w] as i32;
            let up_l = src[i - w - 1] as i32;
            let left = src[i - 1] as i32;
            let right = src[i + 1] as i32;
            let down = src[i + w] as i32;
            let dn_r = src[i + w + 1] as i32;
            let val = (3 * (up + left - right - down) + 10 * (up_l - dn_r)).abs();
            out[i] = val as u16;
        }
    }
}

fn sobel_h_edge_scalar(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let i = y * w + x;
            let up_l = src[i - w - 1] as i32;
            let up_r = src[i - w + 1] as i32;
            let up = src[i - w] as i32;
            let dn_l = src[i + w - 1] as i32;
            let dn_r = src[i + w + 1] as i32;
            let dn = src[i + w] as i32;
            let val = (3 * (up_l + up_r - dn_l - dn_r) + 10 * (up - dn)).abs();
            out[i] = val as u16;
        }
    }
}

/// M-edge AVX2：每 16 像素，i16 向量宽度（梯度值 |3*(v1±v2)+10*v3| ≤ 4080，i16 安全）。
/// 一次处理 16 像素，比 8×i32 版吞吐翻倍，追平 g++ 自动向量化的性能。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sobel_m_edge_avx2(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    use std::arch::x86_64::*;
    for y in 1..(h - 1) {
        let mut x = 1usize;
        // 每行 3 个 tap（左/中/右），各 16 字节。
        while x + 16 <= w - 1 {
            let up_l = super::load16_i16(src, (y - 1) * w + x - 1);
            let up_m = super::load16_i16(src, (y - 1) * w + x);
            let up_r = super::load16_i16(src, (y - 1) * w + x + 1);
            let mid_l = super::load16_i16(src, y * w + x - 1);
            let mid_r = super::load16_i16(src, y * w + x + 1);
            let dn_l = super::load16_i16(src, (y + 1) * w + x - 1);
            let dn_m = super::load16_i16(src, (y + 1) * w + x);
            let dn_r = super::load16_i16(src, (y + 1) * w + x + 1);

            let v1 = _mm256_sub_epi16(up_l, dn_r);
            let v2 = _mm256_sub_epi16(up_r, dn_l);
            let v3 = _mm256_sub_epi16(up_m, dn_m);
            let v4 = _mm256_sub_epi16(mid_l, mid_r);

            // 3*a = a + a*2；10*a = a*8 + a*2（shift+add，避免 mullo 延迟）。
            // v1+v2 最大 510，*3 = 1530；v3 最大 255，*10 = 2550；和 ≤4080，i16 安全。
            let s1 = _mm256_add_epi16(v1, v2);
            let d1 = _mm256_sub_epi16(v1, v2);
            let s3 = _mm256_add_epi16(v3, v4);
            let d3 = _mm256_sub_epi16(v3, v4);

            let mut max = _mm256_abs_epi16(_mm256_add_epi16(
                _mm256_add_epi16(s1, _mm256_slli_epi16(s1, 1)),
                _mm256_add_epi16(_mm256_slli_epi16(v3, 3), _mm256_slli_epi16(v3, 1)),
            ));
            let mut v = _mm256_abs_epi16(_mm256_add_epi16(
                _mm256_add_epi16(d1, _mm256_slli_epi16(d1, 1)),
                _mm256_add_epi16(_mm256_slli_epi16(v4, 3), _mm256_slli_epi16(v4, 1)),
            ));
            max = _mm256_max_epi16(max, v);
            v = _mm256_abs_epi16(_mm256_add_epi16(
                _mm256_add_epi16(s3, _mm256_slli_epi16(s3, 1)),
                _mm256_add_epi16(_mm256_slli_epi16(v1, 3), _mm256_slli_epi16(v1, 1)),
            ));
            max = _mm256_max_epi16(max, v);
            v = _mm256_abs_epi16(_mm256_add_epi16(
                _mm256_add_epi16(d3, _mm256_slli_epi16(d3, 1)),
                _mm256_add_epi16(_mm256_slli_epi16(v2, 3), _mm256_slli_epi16(v2, 1)),
            ));
            max = _mm256_max_epi16(max, v);
            // max 非负 i16 → 直接按 u16 存。
            _mm256_storeu_si256(out.as_mut_ptr().add(y * w + x) as *mut __m256i, max);
            x += 16;
        }
        // 尾部标量。
        while x < w - 1 {
            let i = y * w + x;
            let lt = src[i - w - 1] as i32;
            let rt = src[i - w + 1] as i32;
            let mt = src[i - w] as i32;
            let lm = src[i - 1] as i32;
            let rm = src[i + 1] as i32;
            let mb = src[i + w] as i32;
            let lb = src[i + w - 1] as i32;
            let rb = src[i + w + 1] as i32;
            let val1 = lt - rb;
            let val2 = rt - lb;
            let val3 = mt - mb;
            let val4 = lm - rm;
            let mut max = (3 * (val1 + val2) + 10 * val3).abs();
            let mut v = (3 * (val1 - val2) + 10 * val4).abs();
            if max < v {
                max = v;
            }
            v = (3 * (val3 + val4) + 10 * val1).abs();
            if max < v {
                max = v;
            }
            v = (3 * (val3 - val4) + 10 * val2).abs();
            if max < v {
                max = v;
            }
            out[i] = max as u16;
            x += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sobel_n_edge_avx2(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    use std::arch::x86_64::*;
    for y in 1..(h - 1) {
        let mut x = 1usize;
        // 16×i16：一次 16 像素（对齐 g++ vpmovzxbw/vpaddw 的策略，i16 安全）。
        while x + 16 <= w - 1 {
            let up = super::load16_i16(src, (y - 1) * w + x);
            let up_l = super::load16_i16(src, (y - 1) * w + x - 1);
            let mid_l = super::load16_i16(src, y * w + x - 1);
            let mid_r = super::load16_i16(src, y * w + x + 1);
            let dn = super::load16_i16(src, (y + 1) * w + x);
            let dn_r = super::load16_i16(src, (y + 1) * w + x + 1);
            // t1 = up - dn + mid_l - mid_r；t2 = up_l - dn_r。
            let t1 = _mm256_sub_epi16(_mm256_add_epi16(up, _mm256_sub_epi16(mid_l, mid_r)), dn);
            let t2 = _mm256_sub_epi16(up_l, dn_r);
            // 3*t1 + 10*t2 = (t1 + t1<<1) + (t2<<3 + t2<<1)。
            let a3 = _mm256_add_epi16(t1, _mm256_slli_epi16(t1, 1));
            let a10 = _mm256_add_epi16(_mm256_slli_epi16(t2, 3), _mm256_slli_epi16(t2, 1));
            let val = _mm256_abs_epi16(_mm256_add_epi16(a3, a10));
            // val 非负 i16 → 直接按 u16 存。
            _mm256_storeu_si256(out.as_mut_ptr().add(y * w + x) as *mut __m256i, val);
            x += 16;
        }
        while x < w - 1 {
            let i = y * w + x;
            let up = src[i - w] as i32;
            let up_l = src[i - w - 1] as i32;
            let left = src[i - 1] as i32;
            let right = src[i + 1] as i32;
            let down = src[i + w] as i32;
            let dn_r = src[i + w + 1] as i32;
            out[i] = (3 * (up + left - right - down) + 10 * (up_l - dn_r)).abs() as u16;
            x += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sobel_h_edge_avx2(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    use std::arch::x86_64::*;
    for y in 1..(h - 1) {
        let mut x = 1usize;
        // 16×i16：一次 16 像素。
        while x + 16 <= w - 1 {
            let up_l = super::load16_i16(src, (y - 1) * w + x - 1);
            let up_m = super::load16_i16(src, (y - 1) * w + x);
            let up_r = super::load16_i16(src, (y - 1) * w + x + 1);
            let dn_l = super::load16_i16(src, (y + 1) * w + x - 1);
            let dn_m = super::load16_i16(src, (y + 1) * w + x);
            let dn_r = super::load16_i16(src, (y + 1) * w + x + 1);
            // t1 = up_l+up_r-dn_l-dn_r；t2 = up_m-dn_m。
            let t1 = _mm256_sub_epi16(_mm256_add_epi16(up_l, up_r), _mm256_add_epi16(dn_l, dn_r));
            let t2 = _mm256_sub_epi16(up_m, dn_m);
            let a3 = _mm256_add_epi16(t1, _mm256_slli_epi16(t1, 1));
            let a10 = _mm256_add_epi16(_mm256_slli_epi16(t2, 3), _mm256_slli_epi16(t2, 1));
            let val = _mm256_abs_epi16(_mm256_add_epi16(a3, a10));
            _mm256_storeu_si256(out.as_mut_ptr().add(y * w + x) as *mut __m256i, val);
            x += 16;
        }
        while x < w - 1 {
            let i = y * w + x;
            let up_l = src[i - w - 1] as i32;
            let up_r = src[i - w + 1] as i32;
            let up = src[i - w] as i32;
            let dn_l = src[i + w - 1] as i32;
            let dn_r = src[i + w + 1] as i32;
            let dn = src[i + w] as i32;
            out[i] = (3 * (up_l + up_r - dn_l - dn_r) + 10 * (up - dn)).abs() as u16;
            x += 1;
        }
    }
}

/// N-edge AVX-512：一次 32 像素（zmm，32×i16），对齐 g++ 的 AVX-512 自动向量化。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vbmi")]
unsafe fn sobel_n_edge_avx512(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    use std::arch::x86_64::*;
    // 3-load：每行（up/mid/dn）只 load 一次，用 permutexvar_epi16 派生 ±1 偏移 tap，
    // 减少内存带宽（带宽 bound，实测 6-load→3-load 快 ~2×）。
    // shift_right1 / shift_left1 索引从 static 常量 load（避免运行时 set_epi16 重建）。
    let sr_idx = _mm512_loadu_si512(SR_IDX.as_ptr() as *const std::arch::x86_64::__m512i);
    let sl_idx = _mm512_loadu_si512(SL_IDX.as_ptr() as *const std::arch::x86_64::__m512i);
    for y in 1..(h - 1) {
        let mut x = 1usize;
        while x + 32 <= w - 1 {
            // 内联 load（不能用 #[inline(always)] + #[target_feature]，须直接内联 intrinsics）。
            let up = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add((y - 1) * w + x) as *const __m256i));
            let mid = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(y * w + x) as *const __m256i));
            let dn = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add((y + 1) * w + x) as *const __m256i));
            // up_l = shift_right(up)，lane 0 = 像素 (y-1, x-1)。
            let up_l = _mm512_permutexvar_epi16(sr_idx, up);
            let up_l = _mm512_mask_set1_epi16(up_l, 1, src[(y - 1) * w + x - 1] as i16);
            // mid_l = shift_right(mid)，mid_r = shift_left(mid)。
            let mid_l = _mm512_permutexvar_epi16(sr_idx, mid);
            let mid_l = _mm512_mask_set1_epi16(mid_l, 1, src[y * w + x - 1] as i16);
            let mid_r = _mm512_permutexvar_epi16(sl_idx, mid);
            let mid_r = _mm512_mask_set1_epi16(mid_r, 1 << 31, src[y * w + x + 32] as i16);
            // dn_r = shift_left(dn)，lane 31 = 像素 (y+1, x+32)。
            let dn_r = _mm512_permutexvar_epi16(sl_idx, dn);
            let dn_r = _mm512_mask_set1_epi16(dn_r, 1 << 31, src[(y + 1) * w + x + 32] as i16);

            let t1 = _mm512_sub_epi16(_mm512_add_epi16(up, _mm512_sub_epi16(mid_l, mid_r)), dn);
            let t2 = _mm512_sub_epi16(up_l, dn_r);
            let a3 = _mm512_add_epi16(t1, _mm512_slli_epi16(t1, 1));
            let a10 = _mm512_add_epi16(_mm512_slli_epi16(t2, 3), _mm512_slli_epi16(t2, 1));
            let val = _mm512_abs_epi16(_mm512_add_epi16(a3, a10));
            _mm512_storeu_si512(out.as_mut_ptr().add(y * w + x) as *mut __m512i, val);
            x += 32;
        }
        while x < w - 1 {
            let i = y * w + x;
            let up = src[i - w] as i32;
            let up_l = src[i - w - 1] as i32;
            let left = src[i - 1] as i32;
            let right = src[i + 1] as i32;
            let down = src[i + w] as i32;
            let dn_r = src[i + w + 1] as i32;
            out[i] = (3 * (up + left - right - down) + 10 * (up_l - dn_r)).abs() as u16;
            x += 1;
        }
    }
}

/// H-edge AVX-512：一次 32 像素（zmm，32×i16），3-load（up/dn 各一次 + permute 派生偏移）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vbmi")]
unsafe fn sobel_h_edge_avx512(src: &[u8], w: usize, h: usize, out: &mut [u16]) {
    use std::arch::x86_64::*;
    let sr_idx = _mm512_loadu_si512(SR_IDX.as_ptr() as *const std::arch::x86_64::__m512i);
    let sl_idx = _mm512_loadu_si512(SL_IDX.as_ptr() as *const std::arch::x86_64::__m512i);
    for y in 1..(h - 1) {
        let mut x = 1usize;
        while x + 32 <= w - 1 {
            let up_m = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add((y - 1) * w + x) as *const __m256i));
            let dn_m = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add((y + 1) * w + x) as *const __m256i));
            let up_l = _mm512_permutexvar_epi16(sr_idx, up_m);
            let up_l = _mm512_mask_set1_epi16(up_l, 1, src[(y - 1) * w + x - 1] as i16);
            let up_r = _mm512_permutexvar_epi16(sl_idx, up_m);
            let up_r = _mm512_mask_set1_epi16(up_r, 1 << 31, src[(y - 1) * w + x + 32] as i16);
            let dn_l = _mm512_permutexvar_epi16(sr_idx, dn_m);
            let dn_l = _mm512_mask_set1_epi16(dn_l, 1, src[(y + 1) * w + x - 1] as i16);
            let dn_r = _mm512_permutexvar_epi16(sl_idx, dn_m);
            let dn_r = _mm512_mask_set1_epi16(dn_r, 1 << 31, src[(y + 1) * w + x + 32] as i16);

            let t1 = _mm512_sub_epi16(_mm512_add_epi16(up_l, up_r), _mm512_add_epi16(dn_l, dn_r));
            let t2 = _mm512_sub_epi16(up_m, dn_m);
            let a3 = _mm512_add_epi16(t1, _mm512_slli_epi16(t1, 1));
            let a10 = _mm512_add_epi16(_mm512_slli_epi16(t2, 3), _mm512_slli_epi16(t2, 1));
            let val = _mm512_abs_epi16(_mm512_add_epi16(a3, a10));
            _mm512_storeu_si512(out.as_mut_ptr().add(y * w + x) as *mut __m512i, val);
            x += 32;
        }
        while x < w - 1 {
            let i = y * w + x;
            let up_l = src[i - w - 1] as i32;
            let up_r = src[i - w + 1] as i32;
            let up = src[i - w] as i32;
            let dn_l = src[i + w - 1] as i32;
            let dn_r = src[i + w + 1] as i32;
            let dn = src[i + w] as i32;
            out[i] = (3 * (up_l + up_r - dn_l - dn_r) + 10 * (up - dn)).abs() as u16;
            x += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成确定性测试图（含边缘/平坦/斜线），随机但固定种子。
    fn test_image(w: usize, h: usize) -> Vec<u8> {
        let mut seed = 0x12345678u32;
        let mut img = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                // 确定性伪随机。
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                img[y * w + x] = ((seed >> 24) & 0xff) as u8;
            }
        }
        // 叠加一条垂直边缘。
        for y in 0..h {
            for x in (w / 2)..w {
                img[y * w + x] = img[y * w + x].wrapping_add(80);
            }
        }
        img
    }

    fn assert_scalar_matches_simd(kind: &str) {
        let (w, h) = (37usize, 19usize); // 非 8 的倍数，测尾部标量。
        let src = test_image(w, h);
        let mut scalar = vec![0u16; w * h];
        let simd;
        match kind {
            "m" => {
                sobel_m_edge_scalar(&src, w, h, &mut scalar);
                simd = sobel_m_edge(&src, w, h);
            }
            "n" => {
                sobel_n_edge_scalar(&src, w, h, &mut scalar);
                simd = sobel_n_edge(&src, w, h);
            }
            _ => {
                sobel_h_edge_scalar(&src, w, h, &mut scalar);
                simd = sobel_h_edge(&src, w, h);
            }
        }
        assert_eq!(simd, scalar, "{} edge: SIMD 与标量应逐位一致", kind);
    }

    #[test]
    fn sobel_m_edge_simd_matches_scalar() {
        assert_scalar_matches_simd("m");
    }

    #[test]
    fn sobel_n_edge_simd_matches_scalar() {
        assert_scalar_matches_simd("n");
    }

    #[test]
    fn sobel_h_edge_simd_matches_scalar() {
        assert_scalar_matches_simd("h");
    }

    /// 微型基准：标量 vs SIMD 在 720p 上的耗时对比（非严格基准，仅观察量级差异）。
    #[test]
    #[ignore] // 手动跑：cargo test -p geometry --lib sobel_bench -- --ignored --nocapture
    fn sobel_bench() {
        let (w, h) = (1280usize, 720usize);
        let src = test_image(w, h);
        let runs = 20;
        let mut out = vec![0u16; w * h];
        let mut t = std::time::Instant::now();
        for _ in 0..runs {
            sobel_m_edge_scalar(&src, w, h, &mut out);
        }
        let scalar_ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        t = std::time::Instant::now();
        for _ in 0..runs {
            sobel_m_edge_into(&src, w, h, &mut out);
        }
        let simd_ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        println!("M-edge 720p: scalar {:.2}ms, SIMD {:.2}ms, 加速 {:.1}x", scalar_ms, simd_ms, scalar_ms / simd_ms);

        // N / H edge。
        for (name, scalar_fn, simd_fn) in [
            ("N", sobel_n_edge_scalar as fn(&[u8], usize, usize, &mut [u16]), sobel_n_edge_into as fn(&[u8], usize, usize, &mut [u16])),
            ("H", sobel_h_edge_scalar as fn(&[u8], usize, usize, &mut [u16]), sobel_h_edge_into as fn(&[u8], usize, usize, &mut [u16])),
        ] {
            let t0 = std::time::Instant::now();
            for _ in 0..runs {
                scalar_fn(&src, w, h, &mut out);
            }
            let s_ms = t0.elapsed().as_secs_f64() * 1000.0 / runs as f64;
            let t0 = std::time::Instant::now();
            for _ in 0..runs {
                simd_fn(&src, w, h, &mut out);
            }
            let d_ms = t0.elapsed().as_secs_f64() * 1000.0 / runs as f64;
            println!("{} -edge 720p: scalar {:.2}ms, SIMD {:.2}ms, 加速 {:.1}x", name, s_ms, d_ms, s_ms / d_ms);
        }
    }
}
