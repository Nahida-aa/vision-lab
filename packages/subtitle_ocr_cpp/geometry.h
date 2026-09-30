#pragma once
#include <vector>
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <queue>
#include <limits>
#include <cstring>
#include <tuple>

struct Point { float x, y; };
using Polygon = std::vector<Point>;

// --- Connected component labeling (4-connectivity) ---
// Returns list of components, each as a vector of pixel coordinates
static std::vector<std::vector<std::pair<int,int>>> connectedComponents(const uint8_t* bitmap, int H, int W) {
    std::vector<int> labels(H * W, 0);
    int nextLabel = 1;
    std::vector<std::vector<std::pair<int,int>>> components;

    const int dx[4] = {1, -1, 0, 0};
    const int dy[4] = {0, 0, 1, -1};

    for (int y = 0; y < H; ++y) {
        for (int x = 0; x < W; ++x) {
            if (bitmap[y * W + x] && labels[y * W + x] == 0) {
                // BFS
                std::queue<std::pair<int,int>> q;
                q.push({x, y});
                labels[y * W + x] = nextLabel;
                std::vector<std::pair<int,int>> comp;

                while (!q.empty()) {
                    auto [cx, cy] = q.front(); q.pop();
                    comp.push_back({cx, cy});
                    for (int d = 0; d < 4; ++d) {
                        int nx = cx + dx[d], ny = cy + dy[d];
                        if (nx >= 0 && nx < W && ny >= 0 && ny < H &&
                            bitmap[ny * W + nx] && labels[ny * W + nx] == 0) {
                            labels[ny * W + nx] = nextLabel;
                            q.push({nx, ny});
                        }
                    }
                }
                components.push_back(std::move(comp));
                nextLabel++;
            }
        }
    }
    return components;
}

// --- Extract outer contour of a component using Moore-Neighbor tracing ---
static Polygon traceContour(const std::vector<std::pair<int,int>>& component, int origH, int origW) {
    if (component.empty()) return {};

    // Find top-leftmost pixel
    auto start = *std::min_element(component.begin(), component.end(),
        [](auto& a, auto& b) { return a.second < b.second || (a.second == b.second && a.first < b.first); });

    // Build a lookup set for O(1) membership check
    std::vector<uint8_t> lookup(origH * origW, 0);
    for (auto& p : component) lookup[p.second * origW + p.first] = 1;

    // 8-direction offsets (clockwise from top-left)
    const int dx8[8] = {0, 1, 1, 1, 0, -1, -1, -1};
    const int dy8[8] = {-1, -1, 0, 1, 1, 1, 0, -1};

    Polygon contour;
    int cx = start.first, cy = start.second;
    int prevDir = 6; // start search from direction 6 (west)

    auto isValid = [&](int x, int y) {
        return x >= 0 && x < origW && y >= 0 && y < origH;
    };

    do {
        contour.push_back({(float)cx, (float)cy});
        bool found = false;
        for (int i = 0; i < 8; ++i) {
            int dir = (prevDir + 1 + i) % 8;
            int nx = cx + dx8[dir], ny = cy + dy8[dir];
            if (isValid(nx, ny) && lookup[ny * origW + nx]) {
                cx = nx; cy = ny;
                prevDir = (dir + 4) % 8; // rotate 180 for next search
                found = true;
                break;
            }
        }
        if (!found) break; // isolated pixel
    } while (cx != start.first || cy != start.second);

    return contour;
}

// --- Convex hull (Andrew's monotone chain) ---
static Polygon convexHull(Polygon pts) {
    if (pts.size() <= 3) return pts;
    std::sort(pts.begin(), pts.end(), [](auto& a, auto& b) {
        return a.x < b.x || (a.x == b.x && a.y < b.y);
    });

    auto cross = [](const Point& o, const Point& a, const Point& b) {
        return (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);
    };

    Polygon lower, upper;
    for (auto& p : pts) {
        while (lower.size() >= 2 && cross(lower[lower.size()-2], lower.back(), p) < 0)
            lower.pop_back();
        lower.push_back(p);
    }
    for (auto it = pts.rbegin(); it != pts.rend(); ++it) {
        auto& p = *it;
        while (upper.size() >= 2 && cross(upper[upper.size()-2], upper.back(), p) < 0)
            upper.pop_back();
        upper.push_back(p);
    }
    lower.pop_back();
    upper.pop_back();
    lower.insert(lower.end(), upper.begin(), upper.end());
    return lower;
}

// --- Minimum area bounding rectangle (rotating calipers) ---
struct RotatedRect {
    Point center;
    float width, height, angle; // angle in radians
    Point corners[4];
};

// --- Box points, ordered to match Python rapidocr get_mini_boxes ---
// Python get_mini_boxes (ch_ppocr_v3_det/utils.py):
//   points = sorted(cv2.boxPoints(bounding_box), key=lambda x: x[0])
//   leftMost  = points[:2]  (smallest x)
//   rightMost = points[2:]  (largest  x)
//   leftMost  = sorted(leftMost, key=lambda x: x[1])   (tl, bl)
//   rightMost = sorted(rightMost, key=lambda x: x[1])  (tr, br)
//   box = [leftMost[0], rightMost[0], rightMost[1], leftMost[1]]
//       = [tl, tr, br, bl]  (clockwise, starting at top-left)
//
// C++: we first generate 4 corners from center+angle+w+h, then sort per
// above rule to get tl, tr, br, bl.  This is required so that the
// downstream orderPointsClockwise and warpPerspectiveCrop get consistent
// input quadrilateral.
static Polygon boxPoints(const RotatedRect& r) {
    Polygon pts(4);
    float cosA = std::cos(r.angle), sinA = std::sin(r.angle);
    float hw = r.width / 2, hh = r.height / 2;
    // 4 corners (order doesn't matter yet — we sort below)
    pts[0] = {r.center.x + (-hw*cosA - (-hh)*sinA), r.center.y + (-hw*sinA + (-hh)*cosA)};
    pts[1] = {r.center.x + ( hw*cosA - (-hh)*sinA), r.center.y + ( hw*sinA + (-hh)*cosA)};
    pts[2] = {r.center.x + ( hw*cosA - hh*sinA),    r.center.y + ( hw*sinA + hh*cosA)};
    pts[3] = {r.center.x + (-hw*cosA - hh*sinA),    r.center.y + (-hw*sinA + hh*cosA)};

    // Sort by x to split into leftMost (smallest 2) and rightMost (largest 2)
    std::vector<int> idx = {0, 1, 2, 3};
    std::sort(idx.begin(), idx.end(), [&](int a, int b) {
        return pts[a].x < pts[b].x;
    });
    std::vector<int> leftIdx  = {idx[0], idx[1]};
    std::vector<int> rightIdx = {idx[2], idx[3]};
    // Sort each group by y (smaller y = higher up)
    std::sort(leftIdx.begin(),  leftIdx.end(),  [&](int a, int b) { return pts[a].y < pts[b].y; });
    std::sort(rightIdx.begin(), rightIdx.end(), [&](int a, int b) { return pts[a].y < pts[b].y; });
    int tl = leftIdx[0], bl = leftIdx[1];
    int tr = rightIdx[0], br = rightIdx[1];

    return {pts[tl], pts[tr], pts[br], pts[bl]};
}

static RotatedRect minAreaRect(const Polygon& pts) {
    auto hull = convexHull(pts);
    int n = (int)hull.size();
    if (n <= 2) {
        RotatedRect r;
        r.center = {0,0}; r.width = r.height = 0; r.angle = 0;
        std::fill_n(r.corners, 4, Point{0,0});
        return r;
    }

    auto edgeAngle = [](const Point& a, const Point& b) {
        return std::atan2(b.y - a.y, b.x - a.x);
    };

    float minArea = std::numeric_limits<float>::max();
    RotatedRect best;

    for (int i = 0; i < n; ++i) {
        int j = (i + 1) % n;
        float angle = edgeAngle(hull[i], hull[j]);

        // Project all points onto edge normal and edge direction
        float cosA = std::cos(angle), sinA = std::sin(angle);
        float minProj = std::numeric_limits<float>::max();
        float maxProj = -std::numeric_limits<float>::max();
        float minPerp = std::numeric_limits<float>::max();
        float maxPerp = -std::numeric_limits<float>::max();

        for (auto& p : hull) {
            float proj = p.x * cosA + p.y * sinA;
            float perp = -p.x * sinA + p.y * cosA;
            minProj = std::min(minProj, proj);
            maxProj = std::max(maxProj, proj);
            minPerp = std::min(minPerp, perp);
            maxPerp = std::max(maxPerp, perp);
        }

        float area = (maxProj - minProj) * (maxPerp - minPerp);

        if (area < minArea) {
            minArea = area;
            float cx = (minProj + maxProj) / 2 * cosA - (minPerp + maxPerp) / 2 * sinA;
            float cy = (minProj + maxProj) / 2 * sinA + (minPerp + maxPerp) / 2 * cosA;
            best.center = {cx, cy};
            best.width = maxProj - minProj;
            best.height = maxPerp - minPerp;
            best.angle = angle;
        }
    }

    return best;
}

// --- Polygon area (signed) ---
static float polygonArea(const Polygon& poly) {
    float area = 0;
    int n = (int)poly.size();
    for (int i = 0; i < n; ++i) {
        int j = (i + 1) % n;
        area += poly[i].x * poly[j].y - poly[j].x * poly[i].y;
    }
    return std::abs(area) / 2;
}

// --- Polygon perimeter ---
static float polygonLength(const Polygon& poly) {
    float len = 0;
    int n = (int)poly.size();
    for (int i = 0; i < n; ++i) {
        int j = (i + 1) % n;
        float dx = poly[j].x - poly[i].x, dy = poly[j].y - poly[i].y;
        len += std::sqrt(dx * dx + dy * dy);
    }
    return len;
}

// --- Polygon offset (vertex normal approach) ---
//
// Push each polygon vertex outward along the averaged edge-normal direction.
// This approximates pyclipper.PyclipperOffset(JT_ROUND, ET_CLOSEDPOLYGON) for
// rectangular polygons.  The exact expansion factor is slightly less than
// pyclipper's (distance/sqrt(2) instead of distance), but the NMS post-filter
// compensates for resulting box overlaps.
static Polygon offsetPolygon(const Polygon& poly, float distance) {
    if (poly.empty()) return {};
    int n = (int)poly.size();
    Polygon result;

    float signedArea = 0;
    for (int i = 0; i < n; ++i) {
        int j = (i + 1) % n;
        signedArea += poly[i].x * poly[j].y - poly[j].x * poly[i].y;
    }
    float sign = signedArea >= 0 ? 1.0f : -1.0f;

    for (int i = 0; i < n; ++i) {
        int prev = (i - 1 + n) % n;
        int next = (i + 1) % n;

        float e1x = poly[i].x - poly[prev].x, e1y = poly[i].y - poly[prev].y;
        float e2x = poly[next].x - poly[i].x, e2y = poly[next].y - poly[i].y;

        float len1 = std::sqrt(e1x * e1x + e1y * e1y);
        float len2 = std::sqrt(e2x * e2x + e2y * e2y);
        if (len1 < 1e-6f || len2 < 1e-6f) continue;

        float n1x = sign * e1y / len1, n1y = -sign * e1x / len1;
        float n2x = sign * e2y / len2, n2y = -sign * e2x / len2;

        // Average outward normal
        float nx = n1x + n2x, ny = n1y + n2y;
        float nlen = std::sqrt(nx * nx + ny * ny);
        if (nlen < 1e-6f) {
            nx = n1x; ny = n1y;
            nlen = 1.0f;
        }

        float scale = distance / nlen;
        result.push_back({poly[i].x + nx * scale, poly[i].y + ny * scale});
    }

    return result;
}

// --- Box score (fast version, using local mean) ---
static float boxScore(const float* heatmap, int H, int W, const Polygon& box) {
    // Get bounding box
    float xmin_f = box[0].x, xmax_f = box[0].x;
    float ymin_f = box[0].y, ymax_f = box[0].y;
    for (auto& p : box) {
        xmin_f = std::min(xmin_f, p.x); xmax_f = std::max(xmax_f, p.x);
        ymin_f = std::min(ymin_f, p.y); ymax_f = std::max(ymax_f, p.y);
    }
    int xmin = std::max(0, std::min(W-1, (int)std::floor(xmin_f)));
    int xmax = std::max(0, std::min(W-1, (int)std::ceil(xmax_f)));
    int ymin = std::max(0, std::min(H-1, (int)std::floor(ymin_f)));
    int ymax = std::max(0, std::min(H-1, (int)std::ceil(ymax_f)));

    int bw = xmax - xmin + 1, bh = ymax - ymin + 1;
    if (bw <= 0 || bh <= 0) return 0;

    // Create local mask via point-in-polygon test
    float sum = 0;
    int count = 0;
    for (int y = ymin; y <= ymax; ++y) {
        for (int x = xmin; x <= xmax; ++x) {
            // Point-in-polygon (ray casting)
            bool inside = false;
            int nv = (int)box.size();
            for (int i = 0, j = nv - 1; i < nv; j = i++) {
                if (((box[i].y > y) != (box[j].y > y)) &&
                    (x < (box[j].x - box[i].x) * (y - box[i].y) / (box[j].y - box[i].y) + box[i].x))
                    inside = !inside;
            }
            if (inside) {
                sum += heatmap[y * W + x];
                count++;
            }
        }
    }
    return count > 0 ? sum / count : 0;
}
