// 用 OpenCV 复刻 VideoSubFinder 的「去背景字幕提取」链路（GetTransformedImage → ISA 图）。
//
// 目的：给 Rust 版 subtitle_finder 输出的 `*_mask.png`（ISA 去背景图）一个 C++/OpenCV
// 参照，直观对比两者「去背景字幕图」的效果。
//
// 链路（对齐 VideoSubFinder 思路）：
//   抽帧(BGR) → ColorFiltration(色差找字幕带) → BGR2YUV → Sobel M/N/H 边缘 →
//   直方图阈值化 → ESS/ECP 平滑增强 → 连通域过滤小噪点 → 输出 ISA 图(黑底白字)
//
// 编译：
//   g++ -O3 -march=native -std=c++17 -I/usr/include/opencv5 isa_compare.cpp \
//       -L/usr/lib -lopencv_core -lopencv_imgproc -lopencv_imgcodecs -lopencv_videoio -o isa_compare
//
// 用法：
//   ./isa_compare <video> [start_ms ...]     # 对给定时间点抽帧并输出去背景 ISA 图
//   ./isa_compare <video> -all               # 每 ~400ms 抽一帧处理（快速预览）

#include <opencv2/opencv.hpp>
#include <opencv2/imgproc.hpp>
#include <opencv2/geometry/2d.hpp>
#include <cstdio>
#include <string>
#include <vector>
#include <cstdint>

static void save_isa(cv::Mat& isa, const std::string& path) {
    // ISA 图：黑底白字（255=字幕文字）。
    cv::imwrite(path, isa);
    printf("  已输出: %s\n", path.c_str());
}

// 色差找字幕带（对齐 ColorFiltration 思路）：按行色差标记含"文字带"的行范围。
static void color_filtration(const cv::Mat& bgr, int& ymin, int& ymax) {
    cv::Mat gray;
    cv::cvtColor(bgr, gray, cv::COLOR_BGR2GRAY);
    // 对每行计算水平色差（相邻像素差的绝对值之和），色差大的行视为字幕行。
    std::vector<int> row_diff(gray.rows, 0);
    for (int y = 0; y < gray.rows; y++) {
        const uint8_t* row = gray.ptr<uint8_t>(y);
        long diff = 0;
        for (int x = 1; x < gray.cols; x++) {
            diff += std::abs((int)row[x] - (int)row[x - 1]);
        }
        row_diff[y] = (int)diff;
    }
    // 取色差最大的连续带作为字幕区（简化：找 top-k 行的连续段）。
    ymin = gray.rows; ymax = -1;
    int max_avg = 0;
    int best_y = 0;
    for (int y = 4; y < gray.rows - 4; y++) {
        // 5 行平均色差。
        long s = 0;
        for (int k = -2; k <= 2; k++) s += row_diff[y + k];
        int avg = (int)(s / 5);
        if (avg > max_avg) { max_avg = avg; best_y = y; }
    }
    // 以最佳行上下各扩 20 行作为字幕带（视频里字幕通常在中下部）。
    ymin = std::max(0, best_y - 20);
    ymax = std::min(gray.rows - 1, best_y + 20);
    // 若色差太小（无字幕）则不输出。
    if (max_avg < 200) { ymin = gray.rows; ymax = -1; }
}

// 复刻 ISA 去背景链路：输入 BGR 帧，输出去背景字幕前景图（黑底白字）。
static void extract_isa(const cv::Mat& bgr, cv::Mat& isa_out) {
    int ymin, ymax;
    color_filtration(bgr, ymin, ymax);
    if (ymin >= ymax) {
        isa_out = cv::Mat::zeros(bgr.size(), CV_8UC1);
        return;
    }

    // 只处理字幕带区域。
    cv::Rect roi(0, ymin, bgr.cols, ymax - ymin + 1);
    cv::Mat band = bgr(roi).clone();

    // BGR → YUV，取 Y 亮度。
    cv::Mat yuv, Y;
    cv::cvtColor(band, yuv, cv::COLOR_BGR2YUV);
    cv::extractChannel(yuv, Y, 0);

    // Sobel 边缘（垂直+水平）。
    cv::Mat sobelx, sobely, edge;
    cv::Sobel(Y, sobelx, CV_16S, 1, 0, 3);
    cv::Sobel(Y, sobely, CV_16S, 0, 1, 3);
    cv::convertScaleAbs(sobelx, sobelx);
    cv::convertScaleAbs(sobely, sobely);
    cv::addWeighted(sobelx, 0.5, sobely, 0.5, 0, edge);

    // 阈值化（Otsu）分离文字与背景。
    cv::Mat thr;
    cv::threshold(edge, thr, 0, 255, cv::THRESH_BINARY | cv::THRESH_OTSU);

    // 形态学增强：先膨胀连字、再开运算去小噪点（对齐 ESS/ECP 增强思路）。
    cv::Mat enhanced;
    cv::dilate(thr, enhanced, cv::getStructuringElement(cv::MORPH_RECT, cv::Size(2, 2)));
    cv::morphologyEx(enhanced, enhanced, cv::MORPH_CLOSE, cv::getStructuringElement(cv::MORPH_RECT, cv::Size(3, 2)));

    // 连通域过滤：去掉过小的噪点（对齐 ClearImageFromSmallSymbols）。
    std::vector<std::vector<cv::Point>> contours;
    std::vector<cv::Vec4i> hier;
    cv::findContours(enhanced, contours, hier, cv::RETR_EXTERNAL, cv::CHAIN_APPROX_SIMPLE);
    cv::Mat cleaned = cv::Mat::zeros(enhanced.size(), CV_8UC1);
    for (auto& c : contours) {
        cv::Rect r = cv::boundingRect(c);
        // 保留高度 >= 8px 或宽度 >= 10px 的块（字幕文字尺寸）。
        if (r.height >= 8 && r.width >= 10) {
            cv::drawContours(cleaned, std::vector<std::vector<cv::Point>>{c}, -1, 255, cv::FILLED);
        }
    }

    // 放回全图尺寸（字幕带外为 0）。
    isa_out = cv::Mat::zeros(bgr.size(), CV_8UC1);
    cleaned.copyTo(isa_out(roi));
}

int main(int argc, char** argv) {
    if (argc < 3) {
        printf("用法: %s <video> [start_ms ...] | -all\n", argv[0]);
        return 1;
    }
    std::string video = argv[1];
    cv::VideoCapture cap(video);
    if (!cap.isOpened()) { printf("打不开视频: %s\n", video.c_str()); return 1; }
    double fps = cap.get(cv::CAP_PROP_FPS);
    if (fps <= 0) fps = 30.0;
    double total_frames = cap.get(cv::CAP_PROP_FRAME_COUNT);

    std::vector<double> times;
    if (std::string(argv[2]) == "-all") {
        // 每 ~400ms 抽一帧。
        for (double t = 0; t * fps < total_frames; t += 0.4) times.push_back(t);
    } else {
        for (int i = 2; i < argc; i++) times.push_back(atof(argv[i]));
    }

    printf("视频 %s (%.0f fps, %d 帧)，处理 %d 个时间点\n", video.c_str(), fps, (int)total_frames, (int)times.size());

    for (double t_ms : times) {
        int frame_idx = (int)(t_ms / 1000.0 * fps);
        cap.set(cv::CAP_PROP_POS_FRAMES, frame_idx);
        cv::Mat frame;
        if (!cap.read(frame)) {
            printf("  帧 %d 读取失败\n", frame_idx);
            continue;
        }
        cv::Mat isa;
        extract_isa(frame, isa);
        char path[256];
        snprintf(path, sizeof(path), "/home/aa/repos/ai_ls/vision-lab/tools/perf_compare/isa_out/cpp_%d_%d.png", (int)t_ms, frame_idx);
        save_isa(isa, path);
    }
    return 0;
}
