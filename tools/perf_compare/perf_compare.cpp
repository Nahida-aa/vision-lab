// subtitle_finder 逐像素算子的 C++/OpenCV 性能参照基准。
//
// 目的：给纯 Rust SIMD 版的逐像素算子一个 C++/OpenCV 的性能参照，回答
// 「我们的 Rust 复刻是比原始 C++（VideoSubFinder 用 OpenCV）快还是慢」。
//
// 两部分：
//   1. 自定义核（与 Rust 逐位一致的整数公式，纯 C++ -O3）：sobel_m/n/h、
//      bgr2yuv、aply_ess、aply_ecp、dilate —— 隔离语言/编译器差异。
//   2. OpenCV 参考（cv::cvtColor / cv::dilate / cv::Sobel / cv::findContours）：
//      这些是 VideoSubFinder 真正调用的 OpenCV 函数。
//
// 编译（OpenCV 5）：
//   g++ -O3 -march=native -std=c++17 perf_compare.cpp \
//       $(pkg-config --cflags --libs opencv5) -o perf_compare
//   （若 pkg-config 不可用：-I/usr/include/opencv5 -L/usr/lib -lopencv_core -lopencv_imgproc）

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <vector>
#include <opencv2/opencv.hpp>

using u8 = uint8_t;
using u16 = uint16_t;

// ---------------- 与 Rust 逐位一致的自定义核 ----------------

// ImprovedSobelMEdge
static std::vector<u16> sobel_m(const std::vector<u8>& im, int w, int h) {
    std::vector<u16> out(w * h, 0);
    for (int y = 1; y < h - 1; y++) {
        for (int x = 1; x < w - 1; x++) {
            int i = y * w + x;
            int lt = im[i - w - 1], rt = im[i - w + 1], mt = im[i - w];
            int lm = im[i - 1], rm = im[i + 1], mb = im[i + w];
            int lb = im[i + w - 1], rb = im[i + w + 1];
            int v1 = lt - rb, v2 = rt - lb, v3 = mt - mb, v4 = lm - rm;
            int mx = std::abs(3 * (v1 + v2) + 10 * v3);
            int v = std::abs(3 * (v1 - v2) + 10 * v4);
            if (mx < v) mx = v;
            v = std::abs(3 * (v3 + v4) + 10 * v1);
            if (mx < v) mx = v;
            v = std::abs(3 * (v3 - v4) + 10 * v2);
            if (mx < v) mx = v;
            out[i] = (u16)mx;
        }
    }
    return out;
}

// FastImprovedSobelNEdge
static std::vector<u16> sobel_n(const std::vector<u8>& im, int w, int h) {
    std::vector<u16> out(w * h, 0);
    for (int y = 1; y < h - 1; y++) {
        for (int x = 1; x < w - 1; x++) {
            int i = y * w + x;
            int up = im[i - w], up_l = im[i - w - 1];
            int lf = im[i - 1], rt = im[i + 1], dn = im[i + w], dn_r = im[i + w + 1];
            int val = std::abs(3 * (up + lf - rt - dn) + 10 * (up_l - dn_r));
            out[i] = (u16)val;
        }
    }
    return out;
}

// FastImprovedSobelHEdge
static std::vector<u16> sobel_h(const std::vector<u8>& im, int w, int h) {
    std::vector<u16> out(w * h, 0);
    for (int y = 1; y < h - 1; y++) {
        for (int x = 1; x < w - 1; x++) {
            int i = y * w + x;
            int up_l = im[i - w - 1], up_r = im[i - w + 1], up = im[i - w];
            int dn_l = im[i + w - 1], dn_r = im[i + w + 1], dn = im[i + w];
            int val = std::abs(3 * (up_l + up_r - dn_l - dn_r) + 10 * (up - dn));
            out[i] = (u16)val;
        }
    }
    return out;
}

// BGR -> YUV（OpenCV BGR2YUV 整数公式）
static void bgr2yuv(const std::vector<u8>& bgr, std::vector<u8>& y, std::vector<u8>& u, std::vector<u8>& v, int w, int h) {
    y.resize(w * h); u.resize(w * h); v.resize(w * h);
    for (int i = 0; i < w * h; i++) {
        int b = bgr[i * 3], g = bgr[i * 3 + 1], r = bgr[i * 3 + 2];
        y[i] = (u8)(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        u[i] = (u8)(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
        v[i] = (u8)(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
    }
}

// AplyESS（5x5 高斯）
static std::vector<u16> aply_ess(const std::vector<u16>& im, int w, int h) {
    std::vector<u16> out(w * h, 0);
    for (int y = 2; y < h - 2; y++) {
        for (int x = 2; x < w - 2; x++) {
            int i = y * w + x;
            int64_t val = 2ll * (im[i - 2 * w - 2] + im[i - 2 * w + 2] + im[i + 2 * w - 2] + im[i + 2 * w + 2])
                + 4ll * (im[i - 2 * w - 1] + im[i - 2 * w + 1] + im[i - w - 2] + im[i - w + 2] + im[i + w - 2] + im[i + w + 2] + im[i + 2 * w - 1] + im[i + 2 * w + 1])
                + 5ll * (im[i - 2 * w] + im[i - 2] + im[i + 2] + im[i + 2 * w])
                + 10ll * (im[i - w - 1] + im[i - w + 1] + im[i + w - 1] + im[i + w + 1])
                + 20ll * (im[i - w] + im[i - 1] + im[i + 1] + im[i + w])
                + 40ll * im[i];
            out[i] = (u16)(val / 220);
        }
    }
    return out;
}

// AplyECP（5x5 十字，仅中心非 0）
static std::vector<u16> aply_ecp(const std::vector<u16>& im, int w, int h) {
    std::vector<u16> out(w * h, 0);
    for (int y = 2; y < h - 2; y++) {
        for (int x = 2; x < w - 2; x++) {
            int i = y * w + x;
            if (im[i] == 0) { out[i] = 0; continue; }
            int ii = i - ((w + 1) << 1);
            int64_t val = 8ll * im[ii] + 5ll * im[ii + 1] + 4ll * im[ii + 2] + 5ll * im[ii + 3] + 8ll * im[ii + 4];
            ii += w; val += 5ll * im[ii] + 2ll * im[ii + 1] + im[ii + 2] + 2ll * im[ii + 3] + 5ll * im[ii + 4];
            ii += w; val += 4ll * im[ii] + im[ii + 1] + im[ii + 3] + 4ll * im[ii + 4];
            ii += w; val += 5ll * im[ii] + 2ll * im[ii + 1] + im[ii + 2] + 2ll * im[ii + 3] + 5ll * im[ii + 4];
            ii += w; val += 8ll * im[ii] + 5ll * im[ii + 1] + 4ll * im[ii + 2] + 5ll * im[ii + 3] + 8ll * im[ii + 4];
            out[i] = (u16)(val / 100);
        }
    }
    return out;
}

// 3x3 矩形膨胀（iters 次）
static std::vector<u8> dilate3(const std::vector<u8>& im, int w, int h, int iters) {
    std::vector<u8> cur = im;
    for (int it = 0; it < iters; it++) {
        std::vector<u8> nxt = cur;
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++) {
                if (cur[y * w + x] != 0) {
                    for (int dy = -1; dy <= 1; dy++)
                        for (int dx = -1; dx <= 1; dx++) {
                            int nx = x + dx, ny = y + dy;
                            if (nx >= 0 && ny >= 0 && nx < w && ny < h) nxt[ny * w + nx] = 255;
                        }
                }
            }
        cur.swap(nxt);
    }
    return cur;
}

// ---------------- 基准骨架 ----------------
// 用一个 volatile 累加器防止结果被死代码消除（否则 -O3 会把无副作用的循环优化掉）。

static volatile uint64_t g_sink = 0;

template <typename F>
static double bench_ms(const char* name, int runs, F&& f) {
    auto t0 = std::chrono::high_resolution_clock::now();
    for (int r = 0; r < runs; r++) f();
    auto t1 = std::chrono::high_resolution_clock::now();
    double ms = std::chrono::duration<double, std::milli>(t1 - t0).count() / runs;
    printf("  %-28s : %8.3f ms\n", name, ms);
    return ms;
}

// 把结果累进全局 volatile，防止 DCE。
template <typename T>
static void sink(const std::vector<T>& v) {
    for (size_t i = 0; i < v.size(); i += 997) g_sink += (uint64_t)(uint16_t)v[i];
}
static void sink(const cv::Mat& m) {
    if (m.empty()) return;
    for (int r = 0; r < m.rows; r += 61) g_sink += (uint64_t)m.at<uint8_t>(r, 0);
}
static void sink16(const cv::Mat& m) {
    if (m.empty()) return;
    for (int r = 0; r < m.rows; r += 61) g_sink += (uint64_t)(uint16_t)m.at<int16_t>(r, 0);
}
static void sink_contours(const std::vector<std::vector<cv::Point>>& c) {
    g_sink += c.size();
}

int main() {
    const int w = 1280, h = 720;
    const int runs = 50;

    // 确定性测试图。
    std::vector<u8> gray(w * h);
    std::vector<u8> bgr(w * h * 3);
    uint32_t seed = 0x12345678;
    for (int i = 0; i < w * h; i++) {
        seed = seed * 1664525u + 1013904223u;
        u8 g = (u8)((seed >> 24) & 0xff);
        gray[i] = g;
        bgr[i * 3] = g; bgr[i * 3 + 1] = g; bgr[i * 3 + 2] = g;
    }

    printf("== 自定义核（纯 C++ -O3） ==\n");
    bench_ms("sobel_m (IMOE)", runs, [&]{ auto r = sobel_m(gray, w, h); sink(r); });
    bench_ms("sobel_n (FNOE)", runs, [&]{ auto r = sobel_n(gray, w, h); sink(r); });
    bench_ms("sobel_h (FHOE)", runs, [&]{ auto r = sobel_h(gray, w, h); sink(r); });
    std::vector<u8> y, u, v;
    bench_ms("bgr2yuv (int)", runs, [&]{ bgr2yuv(bgr, y, u, v, w, h); sink(y); });
    // ESS/ECP 输入用 sobel 输出（u16）。
    auto moe = sobel_m(gray, w, h);
    bench_ms("aply_ess", runs, [&]{ auto r = aply_ess(moe, w, h); sink(r); });
    bench_ms("aply_ecp", runs, [&]{ auto r = aply_ecp(moe, w, h); sink(r); });
    bench_ms("dilate 3x3 x4", runs, [&]{ auto r = dilate3(gray, w, h, 4); sink(r); });

    printf("== OpenCV 参考 ==\n");
    cv::Mat cv_bgr(h, w, CV_8UC3, bgr.data());
    cv::Mat cv_gray(h, w, CV_8UC1, gray.data());
    bench_ms("cv::cvtColor BGR2YUV", runs, [&]{ cv::Mat yuv; cv::cvtColor(cv_bgr, yuv, cv::COLOR_BGR2YUV); sink(yuv); });
    bench_ms("cv::dilate 3x3 x4", runs, [&]{ cv::Mat d; cv::dilate(cv_gray, d, cv::Mat(), cv::Point(-1,-1), 4); sink(d); });
    bench_ms("cv::Sobel", runs, [&]{ cv::Mat dx; cv::Sobel(cv_gray, dx, CV_16S, 1, 0, 3); sink16(dx); });
    bench_ms("cv::findContours", runs, [&]{ cv::Mat bin; cv::threshold(cv_gray, bin, 127, 255, cv::THRESH_BINARY); std::vector<std::vector<cv::Point>> c; std::vector<cv::Vec4i> hi; cv::findContours(bin, c, hi, cv::RETR_EXTERNAL, cv::CHAIN_APPROX_SIMPLE); sink_contours(c); });

    printf("\n(g_sink=%llu)\n", (unsigned long long)g_sink);
    return 0;
}
