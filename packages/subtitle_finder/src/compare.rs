//! 跨帧比较：判断两帧字幕内容是否变化（CompareTwoSubs / CompareTwoSubsOptimal）。
//!
//! 复刻 `SSAlgorithms.cpp` 2362-2790：
//! - `CompareTwoSubs`：两帧（与各自 ILA 图交集后）做并集 `ImRES`，按行分成文字带
//!   （`GetLinesInfo`），用 `compare`（`veple`）与 `compare2`（`ilaple`）两个阈值
//!   比较边缘/内容差异，`bln = (val1 | val2) & val3`。
//! - `CompareTwoSubsOptimal`：先试 `CompareTwoSubs`，为 0 时再试 `DifficultCompareTwoSubs2`
//!   （先 `FilterImage` 过滤两帧再比较）。
//!
//! 返回 `true` 表示「字幕内容变化了」。

use super::imgops;
use super::params::Params;
use tracing::{debug, trace};

/// `GetLinesInfo`：找 `im`（==255 白）的水平文字带，合并小带与近带。返回 `Vec<(lb, le)>`。
/// 对齐 SSAlgorithms.cpp 2175-2294（含两轮带合并）。
fn get_lines_info(im: &[u8], w: usize, h: usize, segh: usize) -> Vec<(i32, i32)> {
    // 第一遍：行扫描，找含白的连续行段。
    let mut bln = 0;
    let mut l = 0usize;
    let mut lb = vec![0i32; h];
    let mut le = vec![0i32; h];
    for y in 0..h {
        let ib = y * w;
        let ie = ib + w;
        let mut i = ib;
        let mut found = false;
        while i < ie {
            if im[i] == 255 {
                found = true;
                break;
            }
            i += 1;
        }
        if found {
            if bln == 0 {
                lb[l] = y as i32;
                le[l] = y as i32;
                bln = 1;
            } else {
                le[l] = y as i32;
            }
        } else if bln == 1 {
            bln = 0;
            l += 1;
        }
    }
    if bln == 1 {
        l += 1;
    }
    let ln = l;

    // 合并小带（高度 <= segh）到相邻带。
    let segh = segh as i32;
    let mut l = 0usize;
    let mut ln = ln;
    while (l < ln) && (ln > 1) {
        if (le[l] - lb[l] + 1) <= segh {
            let dn: i32;
            if l == 0 {
                dn = 1;
            } else if l == ln - 1 {
                dn = -1;
            } else {
                let val1 = lb[l] - le[l - 1];
                let val2 = lb[l + 1] - le[l];
                dn = if val2 <= val1 { 1 } else { -1 };
            }
            if dn == 1 {
                lb[l + 1] = lb[l];
                for i in l..(ln - 1) {
                    lb[i] = lb[i + 1];
                    le[i] = le[i + 1];
                }
            } else {
                le[l - 1] = le[l];
                for i in l..(ln - 1) {
                    lb[i] = lb[i + 1];
                    le[i] = le[i + 1];
                }
            }
            ln -= 1;
            continue;
        }
        l += 1;
    }

    // 合并近带（间隙 <= 8 且有一带高度 <= 2*segh）。
    let mut l = 0usize;
    while l + 1 < ln {
        if ((lb[l + 1] - le[l] - 1) <= 8)
            && (((lb[l] - le[l] + 1) <= 2 * segh) || ((lb[l + 1] - le[l + 1] + 1) <= 2 * segh))
        {
            lb[l + 1] = lb[l];
            for i in l..(ln - 1) {
                lb[i] = lb[i + 1];
                le[i] = le[i + 1];
            }
            ln -= 1;
            continue;
        }
        l += 1;
    }

    (0..ln).map(|i| (lb[i], le[i])).collect()
}

/// `compare`（对应 C++ `compare` lambda）：对每个文字带，在 `im_res` 白区域内统计
/// `im_ve1`/`im_ve2` 的差异。返回 `(bln, max_dif)`。bln=1 表示差异 <= `veple`。
#[allow(clippy::too_many_arguments)]
fn compare_ve(
    im_res: &[u8],
    lb: &[i32],
    le: &[i32],
    ln: usize,
    w: usize,
    veple: f32,
    im_ve1: &[u8],
    im_ve2: &[u8],
) -> (bool, f64) {
    let mut max_dif = 0.0f64;
    for k in 0..ln {
        let ib = ((lb[k] + 1) * w as i32) as usize;
        let ie = ((le[k] - 1) * w as i32) as usize;
        let (mut dif1, mut dif2, mut cmb) = (0u64, 0u64, 0u64);
        let mut i = ib;
        while i < ie {
            if im_res[i] == 255 {
                let v1 = im_ve1[i];
                let v2 = im_ve2[i];
                if v1 != v2 {
                    if v1 == 255 {
                        dif1 += 1;
                    } else {
                        dif2 += 1;
                    }
                } else if v1 == 255 {
                    cmb += 1;
                }
            }
            i += 1;
        }
        let dif = if dif2 > dif1 { dif2 } else { dif1 };
        if cmb == 0 {
            max_dif = 10.0;
            debug!(k, dif1, dif2, "compare_ve: 带 cmb=0 => false");
            return (false, max_dif);
        }
        let cur_dif = dif as f64 / cmb as f64;
        if cur_dif > max_dif {
            max_dif = cur_dif;
        }
        if cur_dif > veple as f64 {
            debug!(k, cur_dif, dif1, dif2, cmb, "compare_ve: 带 dif/cmb > veple => false");
            return (false, max_dif);
        }
    }
    (true, max_dif)
}

/// `compare2`（对应 C++ `compare2` lambda）：比较两帧内容差异（ilaple 阈值）。
#[allow(clippy::too_many_arguments)]
fn compare_ila(
    lb: &[i32],
    le: &[i32],
    ln: usize,
    w: usize,
    ilaple: f32,
    im1: &[u8],
    im2: &[u8],
) -> (bool, f64) {
    let mut max_dif = 0.0f64;
    for k in 0..ln {
        let ib = ((lb[k] + 1) * w as i32) as usize;
        let ie = ((le[k] - 1) * w as i32) as usize;
        let (mut dif1, mut dif2, mut cmb) = (0u64, 0u64, 0u64);
        let mut i = ib;
        while i < ie {
            let v1 = im1[i];
            let v2 = im2[i];
            if v1 != v2 {
                if v1 == 255 {
                    dif1 += 1;
                } else {
                    dif2 += 1;
                }
            } else if v1 == 255 {
                cmb += 1;
            }
            i += 1;
        }
        let dif = if dif2 > dif1 { dif2 } else { dif1 };
        if cmb == 0 {
            // 该带 Im1/Im2 无公共白点。诊断：带内 Im1/Im2 各自白点数。
            // 注意 ib..ie 在窄带（le[k]-lb[k] <= 2）时可能 ib > ie（空带，C++ 的
            // `for(i=ib;i<ie;i++)` 对空带安全，此处用 .get() 同样安全，不 panic）。
            let b_im1 = im1.get(ib..ie).map_or(0, |s| s.iter().filter(|&&v| v == 255).count());
            let b_im2 = im2.get(ib..ie).map_or(0, |s| s.iter().filter(|&&v| v == 255).count());
            max_dif = 10.0;
            debug!(k, lb=lb[k], le=le[k], dif1, dif2, cmb, b_im1, b_im2, "compare2: 带 cmb=0 返回 false");
            return (false, max_dif);
        }
        let cur_dif = dif as f64 / cmb as f64;
        if cur_dif > max_dif {
            max_dif = cur_dif;
        }
        if cur_dif > ilaple as f64 {
            debug!(k, lb=lb[k], le=le[k], dif1, dif2, cmb, cur_dif, ilaple, "compare2: dif/cmb > ilaple 返回 false");
            return (false, max_dif);
        }
    }
    (true, max_dif)
}

/// `CompareTwoSubs`：比较两帧字幕。返回 C++ `bln` 语义——**true = 两帧相同（无变化）**，
/// **false = 两帧内容差异大（字幕变化）**。调用方据此判断字幕是否改变。
///
/// `im1`/`im2`：两帧前景文字图（u8）；`ila1`/`ila2`：可选 ILA `u16` 时间图；
/// `ve1`/`ve2`：边缘图（u8）；`ve1b`：可选的第二边缘图（若为 None 则用 `ve1`）。
#[allow(clippy::too_many_arguments)]
pub fn compare_two_subs(
    im1: &[u8],
    ila1: Option<&[u16]>,
    ve1: &[u8],
    ve1b: Option<&[u8]>,
    im2: &[u8],
    ila2: Option<&[u16]>,
    ve2: &[u8],
    w: usize,
    h: usize,
    p: &Params,
) -> bool {
    let veple = p.veple;
    let ilaple = p.ilaple;
    let segh = p.segh;
    // g_use_ILA_images_for_search_subtitles 恒 true：若任一 ILA 存在则走 ILA 分支。
    let ila_active = ila1.is_some() || ila2.is_some();

    // ImRES = (Im1 ∩ ILA1) ∪ (Im2 ∩ ILA2)，或 Im1 ∪ Im2。
    let mut im_ff1 = im1.to_vec();
    let mut im_ff2 = im2.to_vec();
    let im_res: Vec<u8>;
    if ila_active {
        if let Some(ila1) = ila1 {
            imgops::intersect_two_images_inplace(&mut im_ff1, ila1, 0u8);
        }
        if let Some(ila2) = ila2 {
            imgops::intersect_two_images_inplace(&mut im_ff2, ila2, 0u8);
        }
        im_res = imgops::add_two_images(&im_ff1, &im_ff2, w * h);
    } else {
        im_res = imgops::add_two_images(im1, im2, w * h);
    }

    // 找文字带。
    let bands = get_lines_info(&im_res, w, h, segh);
    let ln = bands.len();
    debug!(wc = im_res.iter().filter(|&&v| v == 255).count(), ln, "CompareTwoSubs: im_res");
    if ln == 0 {
        return false;
    }
    let lb: Vec<i32> = bands.iter().map(|&(a, _)| a).collect();
    let le: Vec<i32> = bands.iter().map(|&(_, b)| b).collect();

    // val1 = compare(ImVE1, ImVE2)
    let (val1, _dif1) = compare_ve(&im_res, &lb, &le, ln, w, veple, ve1, ve2);

    // val2 = compare(ImVE12, ImVE2)，仅当 ve1b 与 ve1 指针不同；否则 C++ 置 val2=0（中性）。
    // 布尔下中性 = false，`x || false == x`。
    let (val2, _dif2) = match ve1b {
        Some(ve1b) if ve1b.as_ptr() != ve1.as_ptr() => compare_ve(&im_res, &lb, &le, ln, w, veple, ve1b, ve2),
        _ => (false, 0.0),
    };

    // val3 = compare2（ILA 组合路径），若两 ILA 都在则计算，否则 1（通过）。
    let val3 = if ila1.is_some() && ila2.is_some() {
        let ila1 = ila1.unwrap();
        let ila2 = ila2.unwrap();
        let ve1b = ve1b.unwrap_or(ve1);
        // val3 输入原始白点（诊断 ILA 清空原因）
        let raw_im1 = im1.iter().filter(|&&v| v == 255).count();
        let raw_ila1 = ila1.iter().filter(|&&v| v != 0).count();
        let raw_ve1 = ve1.iter().filter(|&&v| v == 255).count();
        let ff1_wc = im_ff1.iter().filter(|&&v| v == 255).count();
        // 诊断：中屏噪声带（rows 331-339）的内容来源。
        let rows: Vec<usize> = (331..340).collect();
        let mut im1_noise = 0;
        let mut ila1_noise = 0;
        let mut ff1_noise = 0;
        let mut ve1_noise = 0;
        let mut im2_noise = 0;
        let mut ff2_noise = 0;
        for &y in &rows {
            let ib = y * w;
            for x in 0..w {
                let i = ib + x;
                if im1[i] == 255 { im1_noise += 1; }
                if ila1[i] != 0 { ila1_noise += 1; }
                if im_ff1[i] == 255 { ff1_noise += 1; }
                if ve1[i] == 255 { ve1_noise += 1; }
                if im2[i] == 255 { im2_noise += 1; }
                if im_ff2[i] == 255 { ff2_noise += 1; }
            }
        }
        if ff1_noise > 0 || ff2_noise > 0 {
            debug!(im1_noise, ila1_noise, ff1_noise, ve1_noise, im2_noise, ff2_noise, rows = format!("{:?}", rows), "中屏噪声带 331-339 内容");
        }
        // Im1 = ImFF1 ∩ VE1（∩ VE12 if different）
        let mut im1_c = im_ff1.clone();
        imgops::intersect_two_images_inplace(&mut im1_c, ve1, 0u8);
        if ve1b.as_ptr() != ve1.as_ptr() {
            imgops::intersect_two_images_inplace(&mut im1_c, ve1b, 0u8);
        }
        let mut im2_c = im1_c.clone();
        // ImILAInt = ILA1 ∩Y ILA2（时间交叠）
        let mut ila_int = ila1.to_vec();
        imgops::intersect_y_images(&mut ila_int, ila2, p.max_dl_down as i32, p.max_dl_up as i32);
        // Im2 = Im1 ∩ ImILAInt
        imgops::intersect_two_images_inplace(&mut im2_c, &ila_int, 0u8);
        // Im2 = dilate(Im2) ∩ Im1（C++ SSAlgorithms.cpp:2707 `IntersectTwoImages(Im2, Im1)`）。
        let dil = imgops::dilate(&im2_c, w, h, 1);
        im2_c = dil;
        imgops::intersect_two_images_inplace(&mut im2_c, &im1_c, 0u8);
        // Im2 = Im2 ∩ VE2
        imgops::intersect_two_images_inplace(&mut im2_c, ve2, 0u8);
        let wc_im1 = im1_c.iter().filter(|&&v| v == 255).count();
        let wc_im2 = im2_c.iter().filter(|&&v| v == 255).count();
        let wc_ilaint = ila_int.iter().filter(|&&v| v != 0).count();
        let bands_str: Vec<String> = lb.iter().zip(le.iter()).map(|(a, b)| format!("{}-{}", a, b)).collect();
        debug!(
            wc_im1, wc_im2, wc_ilaint, ln, bands = bands_str.join(","),
            "compare2: 带列表"
        );
        let (v3, dif3) = compare_ila(&lb, &le, ln, w, ilaple, &im2_c, &im1_c);
        debug!(
            wc_im1, wc_im2, wc_ilaint, ln, v3, dif3,
            raw_im1, raw_ila1, raw_ve1, ff1_wc,
            "compare2: ILA 掩码白点数"
        );
        v3
    } else {
        true
    };

    // bln = (val1 | val2) & val3（C++ 用 0/1，true/false 等价于或/与）。
    let result = (val1 || val2) && val3;
    debug!(
        val1, val2, val3, result,
        ila1 = ila1.is_some(),
        ila2 = ila2.is_some(),
        "compare_two_subs: val1|val2 & val3"
    );
    result
}

/// `CompareTwoSubsOptimal`：先试 `CompareTwoSubs`，为 false（内容差异）时再试
/// `DifficultCompareTwoSubs2`（先对两帧做 `FilterImage` 过滤再比较）。
/// 返回 C++ `bln` 语义：**true = 相同，false = 字幕变化**。
#[allow(clippy::too_many_arguments)]
pub fn compare_two_subs_optimal(
    im1: &[u8],
    ila1: Option<&[u16]>,
    ve1: &[u8],
    ve1b: Option<&[u8]>,
    im2: &[u8],
    ila2: Option<&[u16]>,
    ve2: &[u8],
    w: usize,
    h: usize,
    min_x: i32,
    max_x: i32,
    p: &Params,
) -> bool {
    let _ = min_x;
    let _ = max_x;
    // 先直接比较。
    if compare_two_subs(im1, ila1, ve1, ve1b, im2, ila2, ve2, w, h, p) {
        return true;
    }
    debug!("compare_optimal: fast=changed, 进入 DifficultCompareTwoSubs2");
    // DifficultCompareTwoSubs2：过滤两帧后再比较。
    // C++ 用 `GetLinesInfo(AddTwoImages(ImF1,ImF2))` 得到真实文字带，再对两帧 FilterImage。
    let im_res = imgops::add_two_images(im1, im2, w * h);
    let bands = get_lines_info(&im_res, w, h, p.segh);
    let lb: Vec<i32> = bands.iter().map(|&(a, _)| a).collect();
    let le: Vec<i32> = bands.iter().map(|&(_, b)| b).collect();
    let n = lb.len();
    let mut ff1 = im1.to_vec();
    let mut ff2 = im2.to_vec();
    // C++ DifficultCompareTwoSubs2（SSAlgorithms.cpp:2313-2326）在 FilterImage 前先把
    // ImFF1/ImFF2 与各自 ILA 图求交（时间掩码），再 FilterImage。Rust 之前漏了这步，
    // 对未掩码帧过滤 → 保留更多噪声 → CompareTwoSubs 更容易误判内容变化 → 段过度切分。
    if let Some(ila1) = ila1 {
        imgops::intersect_two_images_inplace(&mut ff1, ila1, 0u8);
    }
    if let Some(ila2) = ila2 {
        imgops::intersect_two_images_inplace(&mut ff2, ila2, 0u8);
    }
    debug!(
        ff1_after_ila = ff1.iter().filter(|&&v| v == 255).count(),
        ff2_after_ila = ff2.iter().filter(|&&v| v == 255).count(),
        n,
        "difficult: ILA 求交后白点"
    );
    if n > 0 {
        filter_image(&mut ff1, ve1, w, h, p, &lb, &le, n);
        filter_image(&mut ff2, ve2, w, h, p, &lb, &le, n);
    }
    debug!(
        ff1_after_fimg = ff1.iter().filter(|&&v| v == 255).count(),
        ff2_after_fimg = ff2.iter().filter(|&&v| v == 255).count(),
        "difficult: FilterImage 后白点"
    );
    // VideoSubFinder 在 FilterImage 后还有 filter_image（AnalyseImage 逐带过滤）。
    filter_image_analyse(&mut ff1, w, h, p);
    filter_image_analyse(&mut ff2, w, h, p);
    debug!(
        ff1_after_ana = ff1.iter().filter(|&&v| v == 255).count(),
        ff2_after_ana = ff2.iter().filter(|&&v| v == 255).count(),
        "difficult: AnalyseImage 带过滤后白点"
    );

    let res = compare_two_subs(&ff1, ila1, ve1, ve1b, &ff2, ila2, ve2, w, h, p);
    debug!("compare_optimal: difficult res={}", res);
    res
}

/// `FilterImage`：迭代执行 SecondFiltration + ClearImageFromSmallSymbols 直到稳定。
fn filter_image(
    im_f: &mut [u8],
    im_ne: &[u8],
    w: usize,
    h: usize,
    p: &Params,
    lb: &[i32],
    le: &[i32],
    n: usize,
) {
    loop {
        let prev = im_f.to_vec();
        trace!(
            n,
            lb_first = lb.first().copied().unwrap_or(-1),
            le_first = le.first().copied().unwrap_or(-1),
            wc_before = im_f.iter().filter(|&&v| v == 255).count(),
            ne_wc = im_ne.iter().filter(|&&v| v == 255).count(),
            "filter_image: second_filtration 前"
        );
        let res = super::filter::second_filtration(im_f, im_ne, lb, le, n, w, h, p);
        trace!(
            res,
            wc_after_sf = im_f.iter().filter(|&&v| v == 255).count(),
            "filter_image: second_filtration 后"
        );
        if res == 1 {
            super::filter::clear_image_from_small_symbols(im_f, w, h, p);
            trace!(
                wc_after_clear = im_f.iter().filter(|&&v| v == 255).count(),
                "filter_image: clear_image_from_small_symbols 后"
            );
        }
        // 检查是否有变化。
        let mut changed = false;
        for i in 0..w * h {
            if prev[i] != im_f[i] {
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
}

/// VideoSubFinder `DifficultCompareTwoSubs2` 的 `filter_image`：对每个文字带裁剪子图，
/// `AnalyseImage` 判定该带无文字则清空。这是 CompareTwoSubsOptimal 里被我们之前遗漏的一步
/// （对比 SSAlgorithms.cpp 2323-2337）：若缺这一步，噪声带没被清掉，CompareTwoSubs 可能
/// 误判字幕内容变化（bln=0）→ 字幕段被提前结束/不保存。
fn filter_image_analyse(im_f: &mut [u8], w: usize, h: usize, p: &Params) {
    let bands = get_lines_info(im_f, w, h, p.segh);
    for (lb, le) in bands {
        let hh = (le - lb + 1) as usize;
        let sub: Vec<u8> = (lb as usize..=le as usize)
            .flat_map(|y| im_f[y * w..y * w + w].to_vec())
            .collect();
        let arr = ndarray::Array2::from_shape_vec((hh, w), sub).expect("带子图尺寸");
        if !crate::preprocess::analyse_image(&arr, p) {
            // 该带无文字 → 清空。
            for y in (lb as usize)..=(le as usize) {
                im_f[y * w..y * w + w].iter_mut().for_each(|v| *v = 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params::default()
    }

    #[test]
    fn lines_info_finds_and_merges() {
        let (w, h) = (40, 40);
        let mut im = vec![0u8; w * h];
        // 两条文字带：行 2-10 与行 22-30（高度 9，间隙 22-10-1=11 > 8，不合并）。
        for y in 2..11 {
            for x in 5..30 {
                im[y * w + x] = 255;
            }
        }
        for y in 22..31 {
            for x in 5..30 {
                im[y * w + x] = 255;
            }
        }
        let bands = get_lines_info(&im, w, h, 3);
        assert_eq!(bands.len(), 2, "两条带");
        assert_eq!(bands[0], (2, 10));
        assert_eq!(bands[1], (22, 30));
    }

    #[test]
    fn compare_same_frames_true() {
        // 两帧内容相同 → 无变化（C++ bln=true）。
        let (w, h) = (40, 30);
        let mut im = vec![0u8; w * h];
        for y in 5..9 {
            for x in 5..30 {
                im[y * w + x] = 255;
            }
        }
        let ve = vec![255u8; w * h]; // 全边缘
        let res = compare_two_subs(&im, None, &ve, None, &im, None, &ve, w, h, &params());
        assert!(res, "相同帧应返回 true（无变化）");
    }

    #[test]
    fn compare_different_frames_false() {
        // 两帧文字内容不同 → 变化（C++ bln=false）。
        let (w, h) = (40, 30);
        let mut im1 = vec![0u8; w * h];
        let mut im2 = vec![0u8; w * h];
        // im1: 行 5-8 左半；im2: 行 5-8 右半 → 差异大。
        for y in 5..9 {
            for x in 5..20 {
                im1[y * w + x] = 255;
            }
            for x in 20..35 {
                im2[y * w + x] = 255;
            }
        }
        let ve1 = im1.clone();
        let ve2 = im2.clone();
        let res = compare_two_subs(&im1, None, &ve1, None, &im2, None, &ve2, w, h, &params());
        assert!(!res, "不同帧应返回 false（内容变化）");
    }

    #[test]
    fn compare_optimal_different() {
        // 端到端 optimal。
        let (w, h) = (64, 40);
        let mut im1 = vec![0u8; w * h];
        let mut im2 = vec![0u8; w * h];
        for y in 6..10 {
            for x in 8..40 {
                im1[y * w + x] = 255;
            }
            for x in 30..56 {
                im2[y * w + x] = 255;
            }
        }
        let ve1 = im1.clone();
        let ve2 = im2.clone();
        let res = compare_two_subs_optimal(&im1, None, &ve1, None, &im2, None, &ve2, w, h, 0, w as i32 - 1, &params());
        assert!(!res, "不同帧 optimal 应返回 false（内容变化）");
    }
}
