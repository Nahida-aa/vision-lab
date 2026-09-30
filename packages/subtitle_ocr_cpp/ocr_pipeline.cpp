#define _CRT_SECURE_NO_WARNINGS 1

#include <iostream>
#include <fstream>
#include <string>
#include <vector>
#include <algorithm>
#include <cmath>
#include <cstring>
#include <cstdint>
#include <cctype>
#include <chrono>
#include <sstream>
#include <iomanip>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define NOGDI
#define NOMINMAX
#include <windows.h>
#include <shellapi.h>
static std::wstring toWide(const std::string& s) {
    int len = MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, nullptr, 0);
    std::wstring wstr(len, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, &wstr[0], len);
    wstr.pop_back();
    return wstr;
}
static std::string wideToUtf8(const std::wstring& wstr) {
    int len = WideCharToMultiByte(CP_UTF8, 0, wstr.c_str(), -1, nullptr, 0, nullptr, nullptr);
    if (len <= 0) return {};
    std::string s(len, '\0');
    WideCharToMultiByte(CP_UTF8, 0, wstr.c_str(), -1, &s[0], len, nullptr, nullptr);
    s.pop_back();
    return s;
}
#define ORT_PATH(s) toWide(s).c_str()
#else
#define ORT_PATH(s) (s).c_str()
#endif

#include "onnxruntime_cxx_api.h"
#include "image.h"
#include "geometry.h"

#include <opencv2/opencv.hpp>
#if __has_include(<opencv2/geometry.hpp>)
#include <opencv2/geometry.hpp>   // OC5: minAreaRect/boxPoints/getPerspectiveTransform moved here
#endif

// --- Constants ---
constexpr int DET_LIMIT_SIDE = 736;
constexpr int CLS_H = 48;
constexpr int CLS_W = 192;
constexpr int REC_H = 48;
// 注意：Python rapidocr 的 rec resize 无硬上限/下限（img_width = int(48 * ratio)）

constexpr float DET_THRESH = 0.3f;
constexpr float BOX_THRESH = 0.5f;

constexpr float UNCLIP_RATIO = 1.6f;
constexpr int MAX_CANDIDATES = 1000;

/// 读取 OCR_BOX_THRESH 环境变量覆盖检测框阈值（默认 0.5，与 PP-OCR 官方一致）。
/// 用于与 rust 实现（BOX_THRESH=0.6）做对照实验：设 OCR_BOX_THRESH=0.6 即可让
/// cpp 与 rust 用同一阈值，隔离阈值差异对 spurious/CER 的影响。
static float boxThreshFromEnv() {
    if (const char* v = std::getenv("OCR_BOX_THRESH")) {
        float f = (float)std::atof(v);
        if (f > 0.0f && f < 1.0f) return f;
    }
    return BOX_THRESH;
}

struct OcrImgResult {
    std::string text;
    struct OcrBoxResult {
        std::string text;
        float text_confidence;  // 识别置信度（rec 分支平均字符概率），「字认得准不准」
        float box_confidence;   // 检测框得分（det 后处理框内平均概率），「框定位得准不准」
        std::vector<std::vector<int>> box; // 4x2
    };
    std::vector<OcrBoxResult> boxes;
    double charListLoadMs, imageLoadMs, modelLoadMs, detMs, postMs, recMs, totalMs;
    uint64_t timestamp = 0;  // 对齐 Rust FrameResult：单时刻（ms），0=无时间。
                                // 文件名 ms/ms_ms 解析后填入；ms_ms 同一张图产出两个结果。
};

// --- Load char list from JSON ---
// ppocr_keys.json: 6624 个元素 [ "", "'", "疗", ... ] ，第一个是占位
// 解码用字符表：index 0 = blank (CTC 跳过), 1..6623 = 6623 个字符, 6624 = ' ' (space)
// 共 6625 个 token，与 rec 模型输出维度 (N, T, 6625) 一致
static std::vector<std::string> loadCharList(const std::string& path) {
    std::ifstream ifs(path);
    if (!ifs) return {};
    std::string content((std::istreambuf_iterator<char>(ifs)), {});
    if (content.empty()) return {};

    std::vector<std::string> chars;
    size_t i = 0;
    auto skip = [&] { while (i < content.size() && content[i] <= ' ') i++; };
    skip();
    if (i >= content.size() || content[i] != '[') return {};
    i++;

    while (i < content.size()) {
        skip();
        if (i >= content.size() || content[i] == ']') break;
        if (content[i] != '"') return {};
        i++;
        std::string s;
        while (i < content.size() && content[i] != '"') {
            if (content[i] == '\\' && i + 1 < content.size()) i++;
            s += content[i++];
        }
        if (i >= content.size()) return {};
        i++;
        chars.push_back(s);
        skip();
        if (i < content.size() && content[i] == ',') i++;
    }

    if (chars.empty()) return {};
    std::vector<std::string> table;
    table.reserve(chars.size() + 1);
    table.push_back("");
    for (size_t i = 1; i < chars.size(); ++i) table.push_back(chars[i]);
    table.push_back(" ");
    return table;
}

// --- CTC decode ---
struct CTCResult { std::string text; float confidence; };
static CTCResult ctcDecode(const float* logits, int timesteps, int numClasses, const std::vector<std::string>& charList) {
    std::string chars;
    std::vector<float> confs;
    int prev = -1; // -1 = blank
    for (int t = 0; t < timesteps; ++t) {
        const float* row = logits + t * numClasses;
        int maxIdx = 0;
        float maxVal = row[0];
        for (int c = 1; c < numClasses; ++c) {
            if (row[c] > maxVal) { maxVal = row[c]; maxIdx = c; }
        }
        if (maxIdx == 0) { prev = -1; continue; } // blank
        if (maxIdx != prev) {
            if (maxIdx < (int)charList.size()) {
                chars += charList[maxIdx];
                confs.push_back(maxVal);
            }
        }
        prev = maxIdx;
    }
    float avgConf = 0;
    for (float c : confs) avgConf += c;
    if (!confs.empty()) avgConf /= confs.size();
    return {chars, avgConf};
}

// --- Preprocess for detection ---
struct DetPreproc {
    std::vector<float> tensor; // NCHW
    int origH, origW, resizedH, resizedW;
    int yOffset; // for bottom_only: pixels cropped from top before det
};

static DetPreproc preprocessDet(const uint8_t* rgb, int H, int W, bool bottomOnly) {
    DetPreproc out;
    out.yOffset = 0;
    int roiH = H;
    const uint8_t* roiPtr = rgb;

    if (bottomOnly) {
        // 裁剪到底部 40%，跟 Python subtitle-py.py 一致
        out.yOffset = (int)(H * 0.6f);
        roiH = H - out.yOffset;
        roiPtr = rgb + out.yOffset * W * 3;
    }
    out.origH = roiH; out.origW = W;

    // 对齐 Python rapidocr limit_type='min':
    //   if min(h, w) < limit_side_len: scale so shorter side becomes 736
    //   else: keep original size (ratio = 1.0)
    float ratio;
    if (std::min(roiH, W) < DET_LIMIT_SIDE) {
        if (roiH < W) ratio = (float)DET_LIMIT_SIDE / (float)roiH;
        else          ratio = (float)DET_LIMIT_SIDE / (float)W;
    } else {
        ratio = 1.0f;
    }
    int newW = (int)(W * ratio);
    int newH = (int)(roiH * ratio);
    // 对齐 Python: round(resize_h / 32) * 32 (就近取整而非向上取整)
    newW = (int)std::round((float)newW / 32.0f) * 32;
    newH = (int)std::round((float)newH / 32.0f) * 32;
    newW = std::max(32, newW);
    newH = std::max(32, newH);
    out.resizedW = newW; out.resizedH = newH;

    // cv:: 版本：用 OpenCV SIMD bilinear resize 替代手写三重循环
    cv::Mat roi(roiH, W, CV_8UC3, const_cast<uint8_t*>(roiPtr));
    cv::Mat resized;
    cv::resize(roi, resized, cv::Size(newW, newH), 0, 0, cv::INTER_LINEAR);

    out.tensor.resize(3 * newH * newW);
    const float mean[3] = {0.485f, 0.456f, 0.406f};
    const float std[3]  = {0.229f, 0.224f, 0.225f};
    for (int y = 0; y < newH; ++y) {
        uint8_t* row = resized.ptr<uint8_t>(y);
        for (int x = 0; x < newW; ++x) {
            for (int c = 0; c < 3; ++c) {
                float pixel = row[x * 3 + c] / 255.0f;
                out.tensor[c * newH * newW + y * newW + x] = (pixel - mean[c]) / std[c];
            }
        }
    }
    return out;
}

// --- Preprocess for classification ---
static std::vector<float> preprocessCls(const uint8_t* rgb, int H, int W) {
    // 与 Python TextClassifier.resize_norm_img 对齐：等比 resize + zero padding
    float ratio = (float)W / (float)H;
    int resizedW;
    if (std::ceil((float)CLS_H * ratio) > CLS_W) {
        resizedW = CLS_W;
    } else {
        resizedW = (int)std::ceil((float)CLS_H * ratio);
    }
    resizedW = std::max(1, resizedW);

    // cv:: 版本：SIMD bilinear resize
    cv::Mat src(H, W, CV_8UC3, const_cast<uint8_t*>(rgb));
    cv::Mat resized;
    cv::resize(src, resized, cv::Size(resizedW, CLS_H), 0, 0, cv::INTER_LINEAR);

    // zero padding to CLS_W x CLS_H (left-aligned)
    std::vector<float> tensor(3 * CLS_H * CLS_W, 0.0f);
    for (int y = 0; y < CLS_H; ++y) {
        uint8_t* row = resized.ptr<uint8_t>(y);
        for (int x = 0; x < resizedW; ++x) {
            for (int c = 0; c < 3; ++c) {
                float pixel = row[x * 3 + c] / 255.0f;
                tensor[c * CLS_H * CLS_W + y * CLS_W + x] = (pixel - 0.5f) / 0.5f;
            }
        }
    }
    return tensor;
}

// --- Preprocess for recognition ---
struct RecPreproc {
    std::vector<float> tensor;
    int width;
};

static RecPreproc preprocessRec(const uint8_t* rgb, int H, int W) {
    // 与 Python TextRecognizer.resize_norm_img 精确对齐：
    //   img_width = int(img_height * max_wh_ratio)
    //   （int 截断 = 向下取整；无上下限）
    //   resized_w = img_width（当 48*ratio 不是整数时，占绝大多数情况）
    float whRatio = (float)W / (float)H;
    int imgW = (int)(REC_H * whRatio);  // int 截断 = floor（正数值），与 Python 的 int() 一致

    int resizedW;
    if (std::ceil((float)REC_H * whRatio) > (float)imgW) {
        resizedW = imgW;
    } else {
        resizedW = (int)std::ceil((float)REC_H * whRatio);
    }
    resizedW = std::max(1, resizedW);

    // cv:: 版本：SIMD bilinear resize
    cv::Mat src(H, W, CV_8UC3, const_cast<uint8_t*>(rgb));
    cv::Mat resized;
    cv::resize(src, resized, cv::Size(resizedW, REC_H), 0, 0, cv::INTER_LINEAR);

    RecPreproc out;
    out.width = imgW;
    out.tensor.resize(3 * REC_H * imgW, 0.0f);
    for (int y = 0; y < REC_H; ++y) {
        uint8_t* row = resized.ptr<uint8_t>(y);
        for (int x = 0; x < resizedW; ++x) {
            for (int c = 0; c < 3; ++c) {
                float pixel = row[x * 3 + c] / 255.0f;
                out.tensor[c * REC_H * imgW + y * imgW + x] = (pixel - 0.5f) / 0.5f;
            }
        }
    }
    return out;
}

// --- DB post-processing (OpenCV-based; aligned Python rapidocr DBPostProcess) ---
//
// Mirrors the Python implementation in rapidocr_onnxruntime/ch_ppocr_v3_det/utils.py:
//
//   class DBPostProcess:
//       def __call__(self, pred, shape_list):
//           segmentation = pred[0, 0, :, :] > self.thresh
//           mask = cv2.dilate(segmentation, dilation_kernel_2x2_of_ones)
//           contours, _ = cv2.findContours(mask, cv2.RETR_LIST,
//                                           cv2.CHAIN_APPROX_SIMPLE)
//           for contour in contours[:max_candidates]:
//               pts, _ = get_mini_boxes(contour)
//               score = box_score_fast(pred[0, 0, :, :], pts)
//               if score < box_thresh: continue
//               expanded = pyclipper_offset(pts, distance=area*unclip/len)
//               final_box, _ = get_mini_boxes(expanded)
//               final_box *= (dest_width / width, dest_height / height)
//
// C++: we use the equivalent OpenCV functions directly for bit-exact
// behaviour with Python (which also uses OpenCV for all these steps).
static std::vector<std::pair<Polygon, float>> dbPostprocess(
    const float* heatmap, int H, int W, int origH, int origW,
    float thresh, float boxThresh, float unclipRatio, int maxCandidates)
{
    // ------------------------------------------------------------------
    // Step 1. Build cv::Mat view of the probability map (zero-copy).
    // ------------------------------------------------------------------
    // `heatmap` is laid out as NCHW tensor from the detector. For a
    // single-class DB head the shape is (1, 1, H, W).  We take the
    // first element of the batch: heatmap[0, 0, :, :].
    cv::Mat_<float> prob(H, W, const_cast<float*>(heatmap));

    // ------------------------------------------------------------------
    // Step 2. Binary segmentation: bitmap = prob > thresh.
    // ------------------------------------------------------------------
    cv::Mat bitmap;
    cv::compare(prob, thresh, bitmap, cv::CMP_GT);

    // ------------------------------------------------------------------
    // Step 3. 2x2 dilation with all-ones kernel (matches Python
    //         `np.ones((2, 2), dtype=np.uint8)`).
    // ------------------------------------------------------------------
    cv::Mat dilated;
    cv::Mat kernel2x2 = cv::getStructuringElement(cv::MORPH_RECT, {2, 2});
    cv::dilate(bitmap, dilated, kernel2x2);

    // ------------------------------------------------------------------
    // Step 4. cv::findContours — RETR_LIST / CHAIN_APPROX_SIMPLE
    //         (this is the *exact* same call as in Python).
    // ------------------------------------------------------------------
    std::vector<std::vector<cv::Point>> contours;
    std::vector<cv::Vec4i> hierarchy;
    cv::findContours(dilated, contours, hierarchy, cv::RETR_LIST, cv::CHAIN_APPROX_SIMPLE);

    // ------------------------------------------------------------------
    // Step 5-11. Iterate over contours: minAreaRect → box_score →
    //            unclip (pyclipper-style polygon offset) → final box.
    // ------------------------------------------------------------------
    std::vector<std::pair<Polygon, float>> results;
    int numContours = std::min<int>(int(contours.size()), maxCandidates);
    for (int i = 0; i < numContours; ++i) {
        const auto& contour = contours[i];
        if (contour.size() < 3) continue;

        // ---------------- get_mini_boxes(contour) ------------------
        // Python: cv2.minAreaRect → cv2.boxPoints → x-sort → y-sort
        cv::RotatedRect rect = cv::minAreaRect(contour);
        cv::Mat ptsMat;
        cv::boxPoints(rect, ptsMat);
        std::vector<cv::Point2f> boxPtsCV(4);
        for (int k = 0; k < 4; ++k) boxPtsCV[k] = {ptsMat.at<float>(k, 0), ptsMat.at<float>(k, 1)};
        // x-sort then y-sort left/right (Python get_mini_boxes order)
        std::sort(boxPtsCV.begin(), boxPtsCV.end(),
                  [](const cv::Point2f& a, const cv::Point2f& b){ return a.x < b.x; });
        std::vector<cv::Point2f> leftTwo = {boxPtsCV[0], boxPtsCV[1]};
        std::vector<cv::Point2f> rightTwo = {boxPtsCV[2], boxPtsCV[3]};
        std::sort(leftTwo.begin(), leftTwo.end(),
                  [](const cv::Point2f& a, const cv::Point2f& b){ return a.y < b.y; });
        std::sort(rightTwo.begin(), rightTwo.end(),
                  [](const cv::Point2f& a, const cv::Point2f& b){ return a.y < b.y; });
        cv::Point2f tl = leftTwo[0], bl = leftTwo[1];
        cv::Point2f tr = rightTwo[0], br = rightTwo[1];
        // final 4-point polygon — matches Python's `[leftMost[0], rightMost[0], rightMost[1], leftMost[1]]`
        std::vector<cv::Point2f> orderedCV = {tl, tr, br, bl};

        // Early-out degenerate boxes
        float sideA = std::hypot(orderedCV[0].x - orderedCV[1].x, orderedCV[0].y - orderedCV[1].y);
        float sideB = std::hypot(orderedCV[1].x - orderedCV[2].x, orderedCV[1].y - orderedCV[2].y);
        if (std::min(sideA, sideB) < 3.0f) continue;

        // ---------------- box_score_fast(prob, box) --------------
        // Python: create local mask with fillPoly then cv2.mean(prob, mask)
        float score = 0;
        {
            int xmin = INT_MAX, xmax = INT_MIN, ymin = INT_MAX, ymax = INT_MIN;
            for (auto& p : orderedCV) {
                xmin = std::min(xmin, (int)std::floor(p.x));
                xmax = std::max(xmax, (int)std::ceil(p.x));
                ymin = std::min(ymin, (int)std::floor(p.y));
                ymax = std::max(ymax, (int)std::ceil(p.y));
            }
            xmin = std::max(0, std::min(W - 1, xmin));
            xmax = std::max(0, std::min(W - 1, xmax));
            ymin = std::max(0, std::min(H - 1, ymin));
            ymax = std::max(0, std::min(H - 1, ymax));
            int bw = xmax - xmin + 1, bh = ymax - ymin + 1;
            if (bw > 0 && bh > 0) {
                cv::Mat mask(bh, bw, CV_8UC1, cv::Scalar(0));
                std::vector<cv::Point> pts_int;
                pts_int.reserve(4);
                for (auto& p : orderedCV) {
                    pts_int.push_back(cv::Point(int(std::round(p.x - xmin)),
                                                int(std::round(p.y - ymin))));
                }
                std::vector<std::vector<cv::Point>> fillContours{pts_int};
                cv::fillPoly(mask, fillContours, cv::Scalar(1));

                cv::Mat subProb = prob(cv::Rect(xmin, ymin, bw, bh));
                cv::Scalar meanVal = cv::mean(subProb, mask);
                score = (float)meanVal[0];
            }
        }
        if (score < boxThresh) continue;

        // ---------------- unclip(orderedPts, distance) ------------
        // distance = polygon_area * unclip_ratio / perimeter
        // Use handwritten vertex-normal offset (Polygons::offsetPolygon from geometry.h).
        // This is simpler and faster than OpenCV's rasterize+dilate approach, and
        // produces correct 4-point expanded boxes — the output of minAreaRect on
        // a rectangular 4-point offset is the same regardless of corner-rounding
        // method (vertex-normal vs pyclipper JT_ROUND vs cv dilate).
        {
            float area = 0, len = 0;
            for (int k = 0; k < 4; ++k) {
                int j = (k + 1) % 4;
                area += orderedCV[k].x * orderedCV[j].y - orderedCV[j].x * orderedCV[k].y;
                len += std::hypot(orderedCV[j].x - orderedCV[k].x, orderedCV[j].y - orderedCV[k].y);
            }
            area = std::abs(area) * 0.5f;
            float dist = len > 0 ? area * unclipRatio / len : 0;
            dist = std::max(3.0f, dist);

            // Vertex-normal polygon offset: move each corner outward along the
            // angle bisector of the two adjacent edge normals.
            Polygon tmpPoly;
            for (auto& p : orderedCV) tmpPoly.push_back({p.x, p.y});
            Polygon expanded = offsetPolygon(tmpPoly, dist);
            if (expanded.size() < 4) continue;

            // ---------------- get_mini_boxes(expanded) ------------
            std::vector<cv::Point2f> cv_expanded;
            for (auto& p : expanded) cv_expanded.push_back({p.x, p.y});
            cv::RotatedRect finalRect = cv::minAreaRect(cv_expanded);
            cv::Mat finalPtsMat;
            cv::boxPoints(finalRect, finalPtsMat);
            std::vector<cv::Point2f> finalPts(4);
            for (int k = 0; k < 4; ++k)
                finalPts[k] = {finalPtsMat.at<float>(k, 0), finalPtsMat.at<float>(k, 1)};
            float side1 = std::hypot(finalPts[0].x - finalPts[1].x, finalPts[0].y - finalPts[1].y);
            float side2 = std::hypot(finalPts[1].x - finalPts[2].x, finalPts[1].y - finalPts[2].y);
            if (std::min(side1, side2) < 5.0f) continue;

            // Re-x/y sort so output quadrilateral is tl-tr-br-bl
            std::sort(finalPts.begin(), finalPts.end(),
                      [](const cv::Point2f& a, const cv::Point2f& b){ return a.x < b.x; });
            std::vector<cv::Point2f> fl = {finalPts[0], finalPts[1]};
            std::vector<cv::Point2f> fr = {finalPts[2], finalPts[3]};
            std::sort(fl.begin(), fl.end(), [](const cv::Point2f& a, const cv::Point2f& b){ return a.y < b.y; });
            std::sort(fr.begin(), fr.end(), [](const cv::Point2f& a, const cv::Point2f& b){ return a.y < b.y; });
            // tl=fl[0], tr=fr[0], br=fr[1], bl=fl[1]
            Polygon outPoly = {
                {fl[0].x, fl[0].y},
                {fr[0].x, fr[0].y},
                {fr[1].x, fr[1].y},
                {fl[1].x, fl[1].y},
            };

            // ---------------- scale to original image size ---------
            float scaleW = (float)origW / (float)W;
            float scaleH = (float)origH / (float)H;
            for (auto& p : outPoly) {
                p.x = std::round(p.x * scaleW);
                p.y = std::round(p.y * scaleH);
                p.x = std::max(0.0f, std::min((float)origW - 1, p.x));
                p.y = std::max(0.0f, std::min((float)origH - 1, p.y));
            }
            results.push_back({outPoly, score});
        }
    }

    return results;
}

// --- Rotate image 180° ---
static Image rotate180(const Image& img) {
    cv::Mat src(img.h, img.w, CV_8UC3, const_cast<uint8_t*>(img.data.data()));
    cv::Mat flipped;
    cv::flip(src, flipped, -1);  // -1 means flip both axes = 180° rotation
    Image out;
    out.w = img.w; out.h = img.h; out.c = img.c;
    out.data.resize(img.data.size());
    if (flipped.isContinuous()) {
        std::memcpy(out.data.data(), flipped.data, img.data.size());
    } else {
        for (int y = 0; y < img.h; ++y) {
            std::memcpy(out.data.data() + (size_t)y * img.w * 3, flipped.ptr(y), (size_t)img.w * 3);
        }
    }
    return out;
}

// --- Order points clockwise (top-left, top-right, bottom-right, bottom-left) ---
static Polygon orderPointsClockwise(const Polygon& pts) {
    if (pts.size() != 4) return pts;
    std::vector<int> idx(pts.size());
    for (size_t i = 0; i < pts.size(); ++i) idx[i] = (int)i;
    std::sort(idx.begin(), idx.end(), [&](int a, int b) { return pts[a].x < pts[b].x; });

    std::vector<int> leftIdx = {idx[0], idx[1]};
    std::sort(leftIdx.begin(), leftIdx.end(), [&](int a, int b) { return pts[a].y < pts[b].y; });
    int tl = leftIdx[0], bl = leftIdx[1];

    std::vector<int> rightIdx = {idx[2], idx[3]};
    std::sort(rightIdx.begin(), rightIdx.end(), [&](int a, int b) { return pts[a].y < pts[b].y; });
    int tr = rightIdx[0], br = rightIdx[1];

    Polygon out;
    out.push_back(pts[tl]);
    out.push_back(pts[tr]);
    out.push_back(pts[br]);
    out.push_back(pts[bl]);
    return out;
}

// --- Perspective warp crop (Python rapidocr: get_rotate_crop_image) ---
static Image warpPerspectiveCrop(const Image& img, const Polygon& pts) {
    float x0 = pts[0].x, y0 = pts[0].y;
    float x1 = pts[1].x, y1 = pts[1].y;
    float x2 = pts[2].x, y2 = pts[2].y;
    float x3 = pts[3].x, y3 = pts[3].y;

    float w1 = std::sqrt((x1 - x0) * (x1 - x0) + (y1 - y0) * (y1 - y0));
    float w2 = std::sqrt((x2 - x3) * (x2 - x3) + (y2 - y3) * (y2 - y3));
    float h1 = std::sqrt((x3 - x0) * (x3 - x0) + (y3 - y0) * (y3 - y0));
    float h2 = std::sqrt((x2 - x1) * (x2 - x1) + (y2 - y1) * (y2 - y1));

    int dstW = std::max(4, (int)std::round(std::max(w1, w2)));
    int dstH = std::max(4, (int)std::round(std::max(h1, h2)));

    bool rotate90 = (float)dstH / (float)dstW >= 1.5f;
    int outW = rotate90 ? dstH : dstW;
    int outH = rotate90 ? dstW : dstH;

    // Source points (the polygon corners)
    cv::Point2f srcPts[4] = {
        cv::Point2f(x0, y0),
        cv::Point2f(x1, y1),
        cv::Point2f(x2, y2),
        cv::Point2f(x3, y3)
    };
    // Destination rectangle corners (tl, tr, br, bl)
    cv::Point2f dstPts[4] = {
        cv::Point2f(0.0f, 0.0f),
        cv::Point2f((float)(dstW - 1), 0.0f),
        cv::Point2f((float)(dstW - 1), (float)(dstH - 1)),
        cv::Point2f(0.0f, (float)(dstH - 1))
    };

    cv::Mat M = cv::getPerspectiveTransform(srcPts, dstPts);

    // Wrap the source image into a cv::Mat without copying
    cv::Mat src(img.h, img.w, CV_8UC3, const_cast<uint8_t*>(img.data.data()));

    cv::Mat warped;
    cv::warpPerspective(src, warped, M, cv::Size(dstW, dstH), cv::INTER_CUBIC, cv::BORDER_REPLICATE);

    // Handle rotate90: transpose so that text reads horizontally
    if (rotate90) {
        cv::Mat rotated;
        cv::rotate(warped, rotated, cv::ROTATE_90_CLOCKWISE);
        warped = rotated;
    }

    // Copy back to Image struct
    Image out;
    out.w = outW;
    out.h = outH;
    out.c = 3;
    out.data.resize((size_t)outW * outH * 3);
    if (warped.isContinuous()) {
        std::memcpy(out.data.data(), warped.data, (size_t)outW * outH * 3);
    } else {
        for (int y = 0; y < outH; ++y) {
            std::memcpy(out.data.data() + (size_t)y * outW * 3, warped.ptr(y), (size_t)outW * 3);
        }
    }
    return out;
}

// --- Run full OCR pipeline ---
static OcrImgResult runOcr(
    const std::string& imagePath,
    const std::vector<std::string>& charList,
    Ort::Session& detSession,
    Ort::Session& clsSession,
    Ort::Session& recSession,
    Ort::MemoryInfo& memInfo,
    float textScore = 0.5f,
    bool subtitleOnly = false,
    bool useNms = true)
{
    using clock = std::chrono::high_resolution_clock;

    OcrImgResult result;
    result.charListLoadMs = result.imageLoadMs = result.modelLoadMs = 0;
    result.detMs = result.postMs = result.recMs = 0;

    // Load image
    std::cerr << "[OCR] loading image..." << std::endl;
    auto tImg = clock::now();
    auto img = loadImage(imagePath.c_str());
    result.imageLoadMs = std::chrono::duration<double, std::milli>(clock::now() - tImg).count();
    std::cerr << "[OCR] image " << img.w << "x" << img.h << std::endl;

    // --- Detection ---
    auto t0 = clock::now();
    std::cerr << "[OCR] preprocess det..." << std::endl;
    auto detPrep = preprocessDet(img.data.data(), img.h, img.w, subtitleOnly);

    // Det inference
    std::cerr << "[OCR] det inference " << detPrep.resizedH << "x" << detPrep.resizedW << "..." << std::endl;
    std::vector<int64_t> detShape = {1, 3, detPrep.resizedH, detPrep.resizedW};
    Ort::Value detInput = Ort::Value::CreateTensor<float>(
        memInfo, detPrep.tensor.data(), detPrep.tensor.size(), detShape.data(), detShape.size());

    auto detInNames = detSession.GetInputNames();
    auto detOutNames = detSession.GetOutputNames();
    std::vector<const char*> detInputNames = {detInNames[0].c_str()};
    std::vector<const char*> detOutputNames = {detOutNames[0].c_str()};

    auto detOut = detSession.Run(Ort::RunOptions{nullptr}, detInputNames.data(), &detInput, 1,
                                 detOutputNames.data(), 1);
    float* heatmapData = detOut[0].GetTensorMutableData<float>();
    auto detShapeOut = detOut[0].GetTensorTypeAndShapeInfo().GetShape();
    int outH = (int)detShapeOut[2], outW = (int)detShapeOut[3];

    auto t1 = clock::now();
    result.detMs = std::chrono::duration<double, std::milli>(t1 - t0).count();

    // --- DB post-process ---
    // 关键：坐标先映射到 ROI 空间 (detPrep.origH x detPrep.origW)，
    // 然后在下面的 for 循环中 + yOffset 映射回原图（跟 Node.js 一致）
    std::cerr << "[OCR] dbPostprocess " << outH << "x" << outW << " (orig " << detPrep.origH << "x" << detPrep.origW << ")..." << std::endl;
    auto boxesPts = dbPostprocess(heatmapData, outH, outW, detPrep.origH, detPrep.origW,
                                  DET_THRESH, boxThreshFromEnv(), UNCLIP_RATIO, MAX_CANDIDATES);

    // NMS-like overlapping box filter — 非 Python 步骤，但能减少 FP。
    // 用 --no-nms 参数关闭以与 Python 行为完全一致。
    if (useNms && boxesPts.size() > 1) {
        struct BBox { int idx; float xMin, yMin, xMax, yMax, area, score; };
        std::vector<BBox> bboxes;
        bboxes.reserve(boxesPts.size());
        for (size_t i = 0; i < boxesPts.size(); ++i) {
            const auto& poly = boxesPts[i].first;
            if (poly.size() < 4) continue;
            float xMin = poly[0].x, xMax = poly[0].x, yMin = poly[0].y, yMax = poly[0].y;
            for (const auto& p : poly) {
                xMin = std::min(xMin, p.x); xMax = std::max(xMax, p.x);
                yMin = std::min(yMin, p.y); yMax = std::max(yMax, p.y);
            }
            float area = std::max(1.0f, (xMax - xMin)) * std::max(1.0f, (yMax - yMin));
            bboxes.push_back({(int)i, xMin, yMin, xMax, yMax, area, boxesPts[i].second});
        }
        std::sort(bboxes.begin(), bboxes.end(), [](const BBox& a, const BBox& b) { return a.area > b.area; });
        std::vector<bool> keep(boxesPts.size(), true);
        for (size_t i = 0; i < bboxes.size(); ++i) {
            if (!keep[bboxes[i].idx]) continue;
            const BBox& a = bboxes[i];
            for (size_t j = i + 1; j < bboxes.size(); ++j) {
                if (!keep[bboxes[j].idx]) continue;
                const BBox& b = bboxes[j];
                float iXMin = std::max(a.xMin, b.xMin);
                float iYMin = std::max(a.yMin, b.yMin);
                float iXMax = std::min(a.xMax, b.xMax);
                float iYMax = std::min(a.yMax, b.yMax);
                if (iXMax <= iXMin || iYMax <= iYMin) continue;
                float iArea = (iXMax - iXMin) * (iYMax - iYMin);
                if (iArea / b.area > 0.7f) keep[bboxes[j].idx] = false;
            }
        }
        decltype(boxesPts) filtered;
        filtered.reserve(boxesPts.size());
        for (size_t i = 0; i < boxesPts.size(); ++i) {
            if (keep[i]) filtered.push_back(std::move(boxesPts[i]));
        }
        boxesPts = std::move(filtered);
    }

    auto t2 = clock::now();
    result.postMs = std::chrono::duration<double, std::milli>(t2 - t1).count();

    // --- Recognition for each box ---
    int boxIdx = 0;
    for (auto& [boxPts, score] : boxesPts) {
        if (boxPts.size() < 4) continue;

        // box 坐标现在是 ROI 内部的坐标。如果开启了 bottom_only，先加回 yOffset 以便在原图做裁剪
        if (detPrep.yOffset > 0) {
            for (auto& p : boxPts) p.y += (float)detPrep.yOffset;
        }

        // Y-position filter for subtitle only — 对齐 Python rapidocr 比值
        // Python: 620 <= y_center <= 700 on 720p frames → ratio 0.86-0.97
        if (subtitleOnly) {
            float yMinCk = boxPts[0].y, yMaxCk = boxPts[0].y;
            for (auto& p : boxPts) {
                yMinCk = std::min(yMinCk, p.y);
                yMaxCk = std::max(yMaxCk, p.y);
            }
            float yCenter = (yMinCk + yMaxCk) / 2;
            if (yCenter < (float)(img.h * 0.85f) || yCenter > (float)(img.h * 0.99f)) continue;
        }

        // 对齐 Python rapidocr: orderPointsClockwise 排序 + warpPerspective 裁剪
        Polygon orderedPts = orderPointsClockwise(boxPts);
        for (auto& p : orderedPts) {
            p.x = std::max(0.0f, std::min((float)img.w - 1.0f, p.x));
            p.y = std::max(0.0f, std::min((float)img.h - 1.0f, p.y));
        }
        Image crop = warpPerspectiveCrop(img, orderedPts);
        if (crop.w < 4 || crop.h < 4) continue;
        boxPts = orderedPts;

        // Cls inference
        auto clsTensor = preprocessCls(crop.data.data(), crop.h, crop.w);
        std::vector<int64_t> clsShape = {1, 3, CLS_H, CLS_W};
        Ort::Value clsInputVal = Ort::Value::CreateTensor<float>(
            memInfo, clsTensor.data(), clsTensor.size(), clsShape.data(), clsShape.size());

        auto clsInNames = clsSession.GetInputNames();
        auto clsOutNames = clsSession.GetOutputNames();
        std::vector<const char*> clsInputNames = {clsInNames[0].c_str()};
        std::vector<const char*> clsOutputNames = {clsOutNames[0].c_str()};

        auto clsOut = clsSession.Run(Ort::RunOptions{nullptr}, clsInputNames.data(), &clsInputVal, 1,
                                     clsOutputNames.data(), 1);
        float* clsData = clsOut[0].GetTensorMutableData<float>();
        bool rotate = clsData[0] < clsData[1];

        // Rotate if needed
        Image recCrop = rotate ? rotate180(crop) : crop;

        // Rec inference
        auto recPrep = preprocessRec(recCrop.data.data(), recCrop.h, recCrop.w);

        auto t3 = clock::now();
        std::vector<int64_t> recShape = {1, 3, REC_H, recPrep.width};
        Ort::Value recInputVal = Ort::Value::CreateTensor<float>(
            memInfo, recPrep.tensor.data(), recPrep.tensor.size(), recShape.data(), recShape.size());

        auto recInNames = recSession.GetInputNames();
        auto recOutNames = recSession.GetOutputNames();
        std::vector<const char*> recInputNames = {recInNames[0].c_str()};
        std::vector<const char*> recOutputNames = {recOutNames[0].c_str()};

        auto recOut = recSession.Run(Ort::RunOptions{nullptr}, recInputNames.data(), &recInputVal, 1,
                                     recOutputNames.data(), 1);

        auto t4 = clock::now();
        result.recMs += std::chrono::duration<double, std::milli>(t4 - t3).count();

        float* recData = recOut[0].GetTensorMutableData<float>();
        auto recShapeOut = recOut[0].GetTensorTypeAndShapeInfo().GetShape();
        int timesteps = (int)recShapeOut[1];
        int numClasses = (int)recShapeOut[2];

        auto [text, conf] = ctcDecode(recData, timesteps, numClasses, charList);
        if (text.empty()) continue;
        if (conf < textScore) continue;

        // Convert box to integer coordinates for output
        std::vector<std::vector<int>> boxOut;
        for (auto& p : boxPts) {
            boxOut.push_back({(int)std::round(p.x), (int)std::round(p.y)});
        }

        result.boxes.push_back({text, conf, score, std::move(boxOut)});
        boxIdx++;
    }

    // Sort top-to-bottom, left-to-right
    std::sort(result.boxes.begin(), result.boxes.end(),
        [](auto& a, auto& b) {
            auto boxCenterY = [](auto& s) {
                float sum = 0;
                for (auto& p : s.box) sum += p[1];
                return sum / s.box.size();
            };
            auto boxCenterX = [](auto& s) {
                float sum = 0;
                for (auto& p : s.box) sum += p[0];
                return sum / s.box.size();
            };
            float ya = boxCenterY(a), yb = boxCenterY(b);
            if (std::abs(ya - yb) > 20) return ya < yb;
            return boxCenterX(a) < boxCenterX(b);
        });

    // Build full text
    for (auto& box : result.boxes)
        result.text += box.text;

    result.totalMs = result.charListLoadMs + result.imageLoadMs + result.modelLoadMs +
                     result.detMs + result.postMs + result.recMs;

    return result;
}

// --- JSON output ---
static std::string toJson(const OcrImgResult& r, const std::string& filename = "") {
    std::ostringstream ss;
    ss << std::fixed << std::setprecision(2);
    ss << "{\"text\": " << std::quoted(r.text);
    if (!filename.empty()) ss << ", \"file\": " << std::quoted(filename);
    ss << ", \"boxes\": [";
    for (size_t i = 0; i < r.boxes.size(); ++i) {
        auto& box = r.boxes[i];
        ss << "{\"text\": " << std::quoted(box.text)
           << ", \"text_confidence\": " << box.text_confidence
           << ", \"box_confidence\": " << box.box_confidence
           << ", \"box\": [";
        for (size_t j = 0; j < box.box.size(); ++j) {
            ss << "[" << box.box[j][0] << "," << box.box[j][1] << "]";
            if (j + 1 < box.box.size()) ss << ",";
        }
        ss << "]}";
        if (i + 1 < r.boxes.size()) ss << ",";
    }
    ss << "]";
    ss << ", \"timestamp\": " << r.timestamp;
    ss << "}";
    // 耗时是旁路观测数据，不进 JSON（对齐 Rust：由调用方 / tracing 消费）。
    // 这里打到 stderr，方便单图调试，且不污染 stdout 的 JSON 数组。
    std::cerr << "[ocr] file=" << filename
              << " detMs=" << r.detMs
              << " postMs=" << r.postMs
              << " recMs=" << r.recMs
              << " totalMs=" << r.totalMs
              << " (load: charList=" << r.charListLoadMs
              << " image=" << r.imageLoadMs
              << " model=" << r.modelLoadMs << ")\n";
    return ss.str();
}

#include <filesystem>
namespace fs = std::filesystem;

static std::vector<std::string> listFrames(const std::string& dir) {
    std::vector<std::string> files;
#ifdef _WIN32
    auto dirW = toWide(dir);
    std::wstring pattern = dirW + L"\\*";
    WIN32_FIND_DATAW ffd;
    HANDLE hFind = FindFirstFileW(pattern.c_str(), &ffd);
    if (hFind != INVALID_HANDLE_VALUE) {
        do {
            if (ffd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) continue;
            std::wstring fname = ffd.cFileName;
            auto dot = fname.rfind(L'.');
            if (dot == std::wstring::npos) continue;
            std::wstring ext;
            for (auto c : fname.substr(dot)) ext.push_back(towlower(c));
            if (ext != L".jpg" && ext != L".jpeg" && ext != L".png" && ext != L".bmp") continue;
            files.push_back(wideToUtf8(dirW + L"\\" + fname));
        } while (FindNextFileW(hFind, &ffd));
        FindClose(hFind);
    }
#else
    for (const auto& entry : fs::directory_iterator(dir)) {
        if (!entry.is_regular_file()) continue;
        auto path = entry.path().string();
        auto ext = entry.path().extension().string();
        std::transform(ext.begin(), ext.end(), ext.begin(), ::tolower);
        if (ext == ".jpg" || ext == ".jpeg" || ext == ".png" || ext == ".bmp") {
            files.push_back(path);
        }
    }
#endif
    std::sort(files.begin(), files.end());
    return files;
}

static std::string extractFilename(const std::string& path) {
    auto pos = path.find_last_of("/\\");
    return (pos == std::string::npos) ? path : path.substr(pos + 1);
}

// 解析文件名 stem 里的时刻：支持 `ms` 或 `ms_ms`（可前置多余 0），对齐 Rust
// subtitle_ocr 的 parse_name_times。
//   "001234"      → {1234}
//   "001234_1250" → {1234, 1250}
// 仅接受纯十进制数字、1~2 段（'_' 分隔），空段/非数字/超 2 段 → 返回空 vector。
static std::vector<uint64_t> parseNameTimes(const std::string& stem) {
    std::vector<uint64_t> times;
    size_t start = 0;
    int segs = 0;
    for (size_t i = 0; i <= stem.size(); ++i) {
        if (i == stem.size() || stem[i] == '_') {
            ++segs;
            if (segs > 2) return {};          // 超过两段非法
            std::string part = stem.substr(start, i - start);
            if (part.empty()) return {};       // 空段（如 "__"）非法
            for (char c : part) {
                if (!std::isdigit((unsigned char)c)) return {};  // 非纯数字非法
            }
            times.push_back(std::stoull(part));
            start = i + 1;
        }
    }
    if (times.empty()) return {};
    return times;
}

// 取文件名（不含扩展名）的 stem。
static std::string stemOf(const std::string& path) {
    std::string fname = extractFilename(path);
    auto dot = fname.rfind('.');
    if (dot == std::string::npos) return fname;
    return fname.substr(0, dot);
}

static int runMain(int argc, char* argv[]) {
#ifdef _WIN32
    std::vector<std::string> utf8Args;
    std::vector<const char*> utf8Ptrs;
    int wargc;
    LPWSTR* wargv = CommandLineToArgvW(GetCommandLineW(), &wargc);
    if (wargv) {
        for (int i = 0; i < wargc; i++) {
            int len = WideCharToMultiByte(CP_UTF8, 0, wargv[i], -1, nullptr, 0, nullptr, nullptr);
            utf8Args.emplace_back(len, '\0');
            WideCharToMultiByte(CP_UTF8, 0, wargv[i], -1, utf8Args.back().data(), len, nullptr, nullptr);
            utf8Args.back().pop_back();
        }
        LocalFree(wargv);
        argc = (int)utf8Args.size();
        utf8Ptrs.reserve(argc);
        for (auto& s : utf8Args) utf8Ptrs.push_back(s.data());
        argv = const_cast<char**>(utf8Ptrs.data());
    }
#endif
    std::string target;
    float textScore = 0.5f;
    bool subtitleOnly = false;
    bool useNms = true;
    std::string device = "cpu";
    bool dirMode = false;
    // 0 = 未指定，回落到 env / 默认值（见下方 intraOpThreads 解析）
    int intraOpThreads = 0;
    if (argc < 2) {
        std::cerr << "Usage: " << argv[0] << " <image_path|--dir <directory>> [text_score] [--subtitle-only] [--no-nms] [--device cpu|cuda|dml|coreml|rocm] [--threads N]" << std::endl;
        return 1;
    }
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--subtitle-only") == 0) subtitleOnly = true;
        else if (strcmp(argv[i], "--no-nms") == 0) useNms = false;
        else if (strcmp(argv[i], "--device") == 0 && i + 1 < argc) { device = argv[++i]; }
        else if (strcmp(argv[i], "--threads") == 0 && i + 1 < argc) { intraOpThreads = std::atoi(argv[++i]); }
        else if (strcmp(argv[i], "--dir") == 0 && i + 1 < argc) { target = argv[++i]; dirMode = true; }
        else if (argv[i][0] != '-') {
            if (target.empty()) target = argv[i];
            else textScore = std::stof(argv[i]);
        }
    }
    if (target.empty()) {
        std::cerr << "Error: no image path or --dir given" << std::endl;
        return 1;
    }

    try {
        using clock = std::chrono::high_resolution_clock;

        // Find model directory (set by TypeScript wrapper via OCR_MODELS_DIR)
        std::string modelDir;
        const char* envModels = std::getenv("OCR_MODELS_DIR");
        if (!envModels) {
            std::cerr << "OCR_MODELS_DIR not set. The TypeScript wrapper should set this." << std::endl;
            return 1;
        }
        modelDir = envModels;

        // Find char list (set by TypeScript wrapper via OCR_KEYS_PATH)
        std::string keysPath;
        const char* envKeys = std::getenv("OCR_KEYS_PATH");
        if (!envKeys) {
            std::cerr << "OCR_KEYS_PATH not set. The TypeScript wrapper should set this." << std::endl;
            return 1;
        }
        keysPath = envKeys;

        auto t0 = clock::now();
        auto charList = loadCharList(keysPath);
        double charListLoadMs = std::chrono::duration<double, std::milli>(clock::now() - t0).count();
        if (charList.empty()) {
            std::cerr << "Failed to load char list from " << keysPath << std::endl;
            return 1;
        }

        // Initialize ORT
        Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "ocr");
        Ort::SessionOptions sessionOptions;
        // 线程数：CLI --threads 优先，其次环境变量 OCR_INTRA_THREADS，否则默认 4。
        //
        // 默认值 4 是实测最优，不要改成核数自动。det 输入尺寸小、rec 是逐行小图，
        // 算子粒度细，线程超订的调度/同步开销远超并行收益。16 核机器实测
        // （341 帧 / 170.1s，--dir 批量）：
        //   threads=4  → det ~127~155s, rec ~3s,  RTF 0.77~0.93
        //   threads=16 → det ~194s,     rec ~15s, RTF 1.24   ← rec 劣化 5 倍
        if (intraOpThreads <= 0) {
            if (const char* envThreads = std::getenv("OCR_INTRA_THREADS")) {
                intraOpThreads = std::atoi(envThreads);
            }
        }
        if (intraOpThreads <= 0) {
            intraOpThreads = 4;
        }
        sessionOptions.SetIntraOpNumThreads(intraOpThreads);
        std::cerr << "[OCR] intraOpNumThreads=" << intraOpThreads << std::endl;
        sessionOptions.SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_ALL);
        sessionOptions.SetExecutionMode(ExecutionMode::ORT_SEQUENTIAL);

        if (device == "cuda") {
            try {
                OrtCUDAProviderOptions cudaOpts{};
                sessionOptions.AppendExecutionProvider_CUDA(cudaOpts);
                std::cerr << "[OCR] Using CUDA execution provider" << std::endl;
            } catch (const std::exception& e) {
                std::cerr << "[OCR] CUDA EP unavailable (" << e.what() << "), falling back to CPU" << std::endl;
            }
        } else {
            std::cerr << "[OCR] Using CPU execution provider" << std::endl;
        }

        auto memInfo = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);

        // Load models once
        t0 = clock::now();
        auto detPath = modelDir + "/ch_PP-OCRv3_det_infer.onnx";
        auto clsPath = modelDir + "/ch_ppocr_mobile_v2.0_cls_infer.onnx";
        auto recPath = modelDir + "/ch_PP-OCRv3_rec_infer.onnx";

        Ort::Session detSession(env, ORT_PATH(detPath), sessionOptions);
        Ort::Session clsSession(env, ORT_PATH(clsPath), sessionOptions);
        Ort::Session recSession(env, ORT_PATH(recPath), sessionOptions);
        double modelLoadMs = std::chrono::duration<double, std::milli>(clock::now() - t0).count();

        // Build frame list
        std::vector<std::string> framePaths;
        if (dirMode) {
            framePaths = listFrames(target);
        } else {
            framePaths.push_back(target);
        }

        // Output: JSON array, one element per frame (or per 文件名时刻).
        // 文件名约定对齐 Rust FrameResult：
        //   `ms`    → 单结果，timestampMs = 该值
        //   `ms_ms` → 同一张图仅 OCR 一次，产出两个结果（start/end 不同时刻）
        //   其他文件名（单图模式或不符合格式）→ timestampMs = 0
        std::cout << "[";
        bool first = true;
        for (size_t fi = 0; fi < framePaths.size(); ++fi) {
            const auto& fp = framePaths[fi];
            auto result = runOcr(fp, charList, detSession, clsSession, recSession, memInfo,
                                 textScore, subtitleOnly, useNms);
            result.charListLoadMs = charListLoadMs;
            result.modelLoadMs = modelLoadMs;
            result.totalMs = result.charListLoadMs + result.imageLoadMs + result.modelLoadMs +
                             result.detMs + result.postMs + result.recMs;

            std::vector<uint64_t> times;
            if (dirMode) times = parseNameTimes(stemOf(fp));

            if (times.empty()) {
                // 无时刻信息：单结果，timestampMs = 0。
                if (!first) std::cout << ",";
                first = false;
                std::cout << "\n  " << toJson(result, extractFilename(fp));
            } else {
                // 每个时刻产出一个结果（内容相同，仅 timestampMs 不同）；
                // ms_ms 时同一张图 OCR 一次、复制成两个 FrameResult。
                for (uint64_t t : times) {
                    result.timestamp = t;
                    if (!first) std::cout << ",";
                    first = false;
                    std::cout << "\n  " << toJson(result, extractFilename(fp));
                }
            }
        }
        std::cout << "\n]\n";

    } catch (const std::exception& e) {
        std::cerr << "Error: " << e.what() << std::endl;
        return 1;
    }

    return 0;
}

int main(int argc, char* argv[]) {
    try {
        return runMain(argc, argv);
    } catch (const std::exception& e) {
        std::cerr << "Exception: " << e.what() << std::endl;
        return 1;
    }
}
