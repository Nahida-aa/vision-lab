#pragma once
#include <cstdint>
#include <vector>
#include <cmath>
#include <algorithm>
#include <stdexcept>
#include <string>
#include <cstring>
#include <opencv2/opencv.hpp>

#ifdef _WIN32
#include <windows.h>
#endif

struct Image {
    int w, h, c;
    std::vector<uint8_t> data;
};

static Image loadImage(const char* path) {
#ifdef _WIN32
    int wlen = MultiByteToWideChar(CP_UTF8, 0, path, -1, nullptr, 0);
    std::wstring wpath(wlen, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, path, -1, &wpath[0], wlen);
    HANDLE hFile = CreateFileW(wpath.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr,
                                OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (hFile == INVALID_HANDLE_VALUE) {
        throw std::runtime_error("Failed to open image: " + std::string(path));
    }
    LARGE_INTEGER fileSize;
    GetFileSizeEx(hFile, &fileSize);
    std::vector<uint8_t> buf((size_t)fileSize.QuadPart);
    DWORD bytesRead;
    if (!ReadFile(hFile, buf.data(), (DWORD)buf.size(), &bytesRead, nullptr)) {
        CloseHandle(hFile);
        throw std::runtime_error("Failed to read image: " + std::string(path));
    }
    CloseHandle(hFile);
    buf.resize(bytesRead);
    cv::Mat mat = cv::imdecode(buf, cv::IMREAD_COLOR);
#else
    cv::Mat mat = cv::imread(path, cv::IMREAD_COLOR);
#endif

    if (mat.empty()) {
        throw std::runtime_error("Failed to load image: " + std::string(path));
    }

    Image img;
    img.w = mat.cols;
    img.h = mat.rows;
    img.c = 3;
    img.data.resize((size_t)img.w * img.h * 3);

    if (mat.isContinuous()) {
        std::memcpy(img.data.data(), mat.data, (size_t)img.w * img.h * 3);
    } else {
        for (int y = 0; y < img.h; ++y) {
            std::memcpy(img.data.data() + (size_t)y * img.w * 3, mat.ptr(y), (size_t)img.w * 3);
        }
    }
    return img;
}

static float sampleBilinear(const Image& img, float x, float y, int c) {
    // BORDER_REPLICATE: 采样点在图像外时用最近的边缘像素（而非 0）
    // 与 Python cv2.warpPerspective(..., borderMode=BORDER_REPLICATE) 对齐
    if (x < 0) x = 0;
    if (x >= img.w) x = (float)(img.w - 1);
    if (y < 0) y = 0;
    if (y >= img.h) y = (float)(img.h - 1);
    int x0 = std::max(0, std::min((int)x, img.w - 2));
    int y0 = std::max(0, std::min((int)y, img.h - 2));
    int x1 = std::min(x0 + 1, img.w - 1);
    int y1 = std::min(y0 + 1, img.h - 1);
    float fx = x - x0, fy = y - y0;
    auto px = [&](int px, int py) -> float {
        return img.data[(py * img.w + px) * img.c + c];
    };
    return (1-fy)*(1-fx)*px(x0,y0) + (1-fy)*fx*px(x1,y0)
         + fy*(1-fx)*px(x0,y1) + fy*fx*px(x1,y1);
}

// Bicubic interpolation (Catmull-Rom spline, a = -0.5)
// Matches OpenCV INTER_CUBIC behavior for warpPerspective
static float cubicWeight(float t) {
    // Standard bicubic kernel (Keys)
    float tt = t * t;
    float ttt = tt * t;
    // a = -0.5
    if (t < 1.0f)
        return 1.5f * ttt - 2.5f * tt + 1.0f;
    if (t < 2.0f)
        return -0.5f * ttt + 2.5f * tt - 4.0f * t + 2.0f;
    return 0.0f;
}

static float sampleBicubic(const Image& img, float x, float y, int c) {
    // BORDER_REPLICATE: replicate edge pixels
    int xi = (int)x;
    int yi = (int)y;
    float dx = x - xi;
    float dy = y - yi;

    float sum = 0.0f;
    float wsum = 0.0f;

    for (int j = -1; j <= 2; ++j) {
        float wy = cubicWeight(std::fabs((float)j - dy));
        if (wy == 0.0f) continue;
        int sy = yi + j;
        if (sy < 0) sy = 0;
        if (sy >= img.h) sy = img.h - 1;

        for (int i = -1; i <= 2; ++i) {
            float wx = cubicWeight(std::fabs((float)i - dx));
            if (wx == 0.0f) continue;
            int sx = xi + i;
            if (sx < 0) sx = 0;
            if (sx >= img.w) sx = img.w - 1;

            float pixel = img.data[(sy * img.w + sx) * img.c + c];
            sum += wy * wx * pixel;
            wsum += wy * wx;
        }
    }

    if (wsum == 0.0f) return 0.0f;
    float val = sum / wsum;
    if (val < 0.0f) val = 0.0f;
    if (val > 255.0f) val = 255.0f;
    return val;
}

static void resizeBilinear(const uint8_t* src, int sw, int sh, uint8_t* dst, int dw, int dh) {
    for (int dy = 0; dy < dh; ++dy) {
        float sy = (dy + 0.5f) * sh / dh - 0.5f;
        sy = std::max(0.0f, std::min<float>(sh - 1, sy));
        int sy0 = (int)sy, sy1 = std::min(sy0 + 1, sh - 1);
        float fy = sy - sy0;
        for (int dx = 0; dx < dw; ++dx) {
            float sx = (dx + 0.5f) * sw / dw - 0.5f;
            sx = std::max(0.0f, std::min<float>(sw - 1, sx));
            int sx0 = (int)sx, sx1 = std::min(sx0 + 1, sw - 1);
            float fx = sx - sx0;
            for (int c = 0; c < 3; ++c) {
                float v = (1-fy)*(1-fx)*src[(sy0*sw+sx0)*3+c]
                        + (1-fy)*fx   *src[(sy0*sw+sx1)*3+c]
                        + fy   *(1-fx)*src[(sy1*sw+sx0)*3+c]
                        + fy   *fx   *src[(sy1*sw+sx1)*3+c];
                dst[(dy*dw+dx)*3+c] = (uint8_t)std::round(v);
            }
        }
    }
}
