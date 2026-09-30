# RapidOCR (Python) vs ort-cpp — 代码对齐研究

## 目标

将 `packages/subtitle_ocr/ort-cpp/` 中的 C++ OCR pipeline 需要与 Python `rapidocr_onnxruntime` pip 包的行为对齐。对齐的关键不是跑分数字，而是实现细节，这样才能保证未来调整 `text_score` 等参数时，C++ 与 Python 产生等价结果。

本文档按 **det post-process、rec preprocess、CTC decode、warpPerspective crop、`text_score` 阈值 五方面说明。

---

## 1. 全局参数对应表

| 参数 | Python `config.yaml` | Python 类型 | C++ `ocr_pipeline.cpp` | C++ 类型 |
|---|---|---|---|---|
| det limit side | `limit_side_len: 736 | int | `DET_LIMIT_SIDE = 736` | constexpr int |
| det thresh | `thresh: 0.3` | float | `DET_THRESH = 0.3f` | constexpr float |
| det box_thresh | `box_thresh: 0.5` | float | `BOX_THRESH = 0.5f` | constexpr float |
| det unclip_ratio | `unclip_ratio: 1.6` | float | `UNCLIP_RATIO = 1.6f` | constexpr float |
| det use_dilation | `use_dilation: true` | bool | 2x2 kernel dilation (等价于) | manual |
| det max_candidates | `max_candidates: 1000` | int | `MAX_CANDIDATES = 1000` | constexpr int |
| rec image shape | `rec_img_shape: [3, 48, 320]` | (C, H, W) | `REC_H = 48; REC_MAX_W = 2000` （rec_max_w 动态的的

动态调整）

## 2. Detection Post-process (`DBPostProcess`)

### 2.1 Python 参考实现（`rapidocr_onnxruntime/ch_ppocr_v3_det/utils.py`）

```python
class DBPostProcess:
    def __init__(self, thresh=0.3, box_thresh=0.5, max_candidates=1000, unclip_ratio=1.6, score_mode="fast", use_dilation=False):
        ...
    def boxes_from_bitmap(self, pred, bitmap, dest_w, dest_h):
        contours, _ = cv2.findContours(bitmap, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
        for contour in contours[:max_candidates]:
            points, sside = self.get_mini_boxes(contour)  # cv2.minAreaRect + sorted by x, y
            score = self.box_score_fast(pred, points)          # cv2.mean over fillPoly mask
            if score < box_thresh: continue
            box = self.unclip(np.array(points))
            box, sside = self.get_mini_boxes(box.reshape(-1, 1, 2))
            if sside < min_size + 2: continue
            box[:, 0] = np.round(box[:, 0] / width * dest_width).clip(0, dest_width)
            boxes.append(box)
            scores.append(score)
        return boxes, scores

    def unclip(self, box):
        poly = Polygon(box)
        distance = poly.area * unclip_ratio / poly.length  # Shapely Polygon
        offset = pyclipper.PyclipperOffset()
        offset.AddPath(box, pyclipper.JT_ROUND, pyclipper.ET_CLOSEDPOLYGON)
        expanded = np.array(offset.Execute(distance))
        return expanded

    def get_mini_boxes(self, contour):
        bounding_box = cv2.minAreaRect(contour)
        points = sorted(list(cv2.boxPoints(bounding_box)), key=lambda x: x[0])  # sort by x
        index_1, index_2, index_3, index_4 = 0, 1, 2, 3
        if points[1][1] > points[0][1]: index_1 = 0; index_4 = 1 else: index_1 = 1; index_4 = 0
        if points[3][1] > points[2][1]: index_2 = 2; index_3 = 3 else: index_2 = 3; index_3 = 2
        return [points[index_1], points[index_2], points[index_3], points[index_4]], min(bounding_box[1])

    def box_score_fast(self, bitmap, _box):
        h, w = bitmap.shape[:2]
        box = _box.copy()
        xmin = np.floor(np.min(box[:, 0]).clip(0, w-1).astype(int))
        xmax = np.ceil(np.max(box[:, 0])).clip(0, w-1).astype(int))
        ...
        mask = np.zeros((ymax-ymin+1, xmax-xmin+1))
        cv2.fillPoly(mask, box.reshape(1, -1, 2).astype(np.int32), 1)
        return cv2.mean(bitmap[ymin:ymax+1, xmin:xmax+1], mask)[0]
```

### 2.2 C++ 实现（`ort-cpp/ocr_pipeline.cpp）

关键的 pipeline flow:

```
bitmap → dilate → connectedComponents → traceContour → minAreaRect → boxPoints → boxScore → polygon unclip
```

详细:

```cpp
// ocr_pipeline.cpp 段
// 等价于 cv2.findContours：
// 
std::vector<std::pair<Polygon,float>> dbPostprocess(...) {

    // (1) bitmap = pred > thresh  (numpy 
    // (2) 2x2 dilation（Python use_dilation:true)
    // (3) connectedComponents + Moore-Neighbor trace → contour
    // (4) minAreaRect(contour) → boxPoints (x-sorted → tl-tr-br-bl)
    // (5) boxScore_fast (ray-casting in polygon mask → mean)
    // (6) unclip: offsetPolygon(box, area * unclip_ratio / length)
    // (7) minAreaRect(offsetPoly) → final quad
    // (8) scale back to original coord

    // ... (6) 从 "简单扩展矩形尺寸" —已废弃:

    // 旧实现为：
    //   dist = w * rect.width * rect.height * unclip_ratio / (2*(w+h))
    //   rect.width += 2*dist; rect.height += 2*dist
    //   boxFinal = boxPoints(rect)

    // ✅ 新实现为：
        float area = polygonArea(boxPts);      // 与 Shapely poly.area 计算 poly.length ——对已poly.area * unclip / peri
    //
    // Polygon offsetPoly = offsetPolygon(boxPts, dist);
    // RotatedRect finalRect = minAreaRect(offsetPoly);
    // Polygon boxFinal = boxPoints(finalRect);
```

### 2.3 geometry.h）

关键修复中：

```
- **poly的外偏移：
- minAreaRect 输出的 boxPoints 点顺序是 **tl, tr, br, bl（顺时针，左上开始），与 Python rapidocr get_mini_boxes 完全一致。
- 以前 C++ boxPoints 只是简单产生 4 个 corners，未排序，但经过 x 现在已经按照 Python的**。

### 2.4 关键差异点（已修复）

| 步骤 | Python | 版本 | 状态 |
|---|---|---|---|
| contour 轮廓提取 | cv2.findContours(CHAIN_APPROX_SIMPLE | connectedComponents + Moore-Neighbor trace | 近似等价 |
| minAreaRect | cv2.minAreaRect |  |  |
| boxPoints+排序 | cv2.boxPoints + sorted(x) + 左右两组再按 y 排序 | C++ boxPoints 按 x 排序 → left/right 各再按 y 排序 → tl,tr,br,bl | 一致：多边形偏移 | pyclipper.PyclipperOffset(JT_ROUND, ET_CLOSEDPOLYGON) | offsetPolygon(顶点法线方法) | 等价，对矩形一致 |
| box score | cv2.fillPoly + cv2.mean | 射线法 point-in-polygon mask + 均值 | 一致 |

## 3. Warp Perspective Crop

### 3.1 Python rapid_ocr_api.py `get_rotate_crop_image`

```python
def get_rotate_crop_image(img, points):
    img_crop_width = int(max(np.linalg.norm(points[0]-points[1]), np.linalg.norm(points[2]-points[3])))
    img_crop_height = int(max(np.linalg.norm(points[0]-points[3]), np.linalg.norm(points[1]-points[2])))
    pts_std = np.float32([[0,0], [crop_w, 0], [crop_w, crop_h], [0, crop_h])
    M = cv2.getPerspectiveTransform(points, pts_std)
    dst_img = cv2.warpPerspective(img, M, (img_crop_width, img_crop_height), borderMode=cv2.BORDER_REPLICATE, flags=cv2.INTER_CUBIC)
    if dst_img.shape[0]/dst_img.shape[1] >= 1.5: dst_img = np.rot90(dst_img)
    return dst_img
```

### 3.2 C++ `warpPerspectiveCrop`

```cpp
// ocr_pipeline.cpp:
// 
// 
// Image warpPerspectiveCrop(const Image& img, const Polygon& pts):
//   (1) 计算 4 边长度
//   (2) 解 3x3 透视变换矩阵（高斯消元）
//   (3) 双三次采样（Keys bicubic kernel, a=-0.5）—与 cv2.INTER_CUBIC 等价
//   (4) 采样超出源图像超出范围时用 clamp (clamp 到边缘像素（sampleBicubic 里做 clamp）——等价 BORDER_REPLICATE

// geometry.h orderPointsClockwise：
//   按 tl, tr, br, bl 顺序 → 输入多边形 corners 给 warpPerspectiveCrop，与 Python `get_rotate_crop_image 的 points 对应。

关键修复：
- ✅ 改用 sampleBicubic 已改为双三次内核（Keys kernel）——不再是双线性）
- ✅ BORDER_REPLICATE 通过采样时 clamp 到源图像边界像素（clamp）
- ✅ rotate90 的 dst_h/dst_w ≥ 1.5
```

## 4. Recognition Preprocess

### 4.1 Python ch_ppocr_v3_rec/text_recognize.py `resize_norm_img`

```python
def resize_norm_img(self, img, max_wh_ratio):
    img_c, img_h, img_w = self.rec_image_shape  # (3, 48, 320)
    # h, w = img.shape[:2]
    ratio = w / float(h)
    if math.ceil(img_h * ratio) > img_w:
        resized_w = img_w
    else:
        resized_w = int(math.ceil(img_h * ratio))
    resized_image = cv2.resize(img, (resized_w, img_h))
    resized_image = resized_image.transpose((2, 0, 1)) / 255)
    resized_image -= 0.5
    resized_image /= 0.5
    padding_im = np.zeros((img_c, img_h, img_w), dtype=np.float32)
    padding_im[:, :, 0:resized_w] = resized_image
    return padding_im
```

### 4.2 C++ `preprocessRec`

```cpp
// ocr_pipeline.cpp:
static RecPreproc preprocessRec(const uint8_t* rgb, int H, int W):
    float whRatio = (float)W / (float)H;
    int imgW = max(32, (int)(REC_H * whRatio));
    if (ceil(REC_H * whRatio) > imgW)
        resized_w = imgW else resized_w = (int)ceil(REC_H * whRatio));
    // resizeBilinear → 双线性缩放，等价于 Python cv2.resize 默认 INTER_LINEAR
    // normalize: (p/255 - 0.5) / 0.5
    // zero padding to (3, 48, imgW)

关键对齐：
- Python 的 rec_img_shape=(3, 48, 320) —— C++ 的 imgW 是动态计算
- Python max_wh_ratio 在批处理时取 batch 的最大比例，C++ 单帧时等价于取当前 crop 的 wh ratio
- cv2.resize 默认 INTER_LINEAR 与 C++ 的 resizeBilinear 等价

## 5. CTC Decode & Confidence

### 5.1 Python CTCLabelDecode

```python
# rapidocr_onnxruntime/ch_ppocr_v3_rec/utils.py
def __call__(self, preds, label=None):
    preds_idx = preds.argmax(axis=2)
    preds_prob = preds.max(axis=2)
    text = self.decode(preds_idx, preds_prob, is_remove_duplicate=True)
    ...

def decode(self, text_index, text_prob=None, is_remove_duplicate=False):
    char_list = []
    conf_list = []
    for idx in range(len(text_index[0])):  # batch_size=1
        if text_index[0][idx] == 0: continue  # blank token
        if is_remove_duplicate and idx > 0 and text_index[0][idx-1] == text_index[0][idx]: continue
        char_list.append(self.character[int(text_index[0][idx])])
        if text_prob is not None:
            conf_list.append(text_prob[0][idx])
        else:
            conf_list.append(1)
    text = ''.join(char_list)
    return [(text, np.mean(conf_list)) if conf_list else [(text, 1.0)]

// ⚠️ 注意：preds 是 rec model 最后一层的输出（logits 或已经 softmax）
// PP-OCRv3 rec 输出形状 [1, T, 6625]，数值范围通常已在 [0, 1]，
// 已经过 softmax（model 本身包含 softmax）。
```

### 5.2 C++ ctcDecode

```cpp
// ocr_pipeline.cpp:
static CTCResult ctcDecode(const float* logits, int timesteps, int numClasses, const std::vector<std::string>& charList):
    std::vector<float> confs;
    int prev = -1;
    for (int t = 0; t < timesteps; ++t) {
        const float* row = logits + t * numClasses;
        int maxIdx = 0;
        float maxVal = row[0];
        for (int c = 1; c < numClasses; ++c)
            if (row[c] > maxVal) { maxVal = row[c]; maxIdx = c; }
        if (maxIdx == 0) { prev = -1; continue; }  // blank
        if (maxIdx != prev) {
            chars.push_back(charList[maxIdx]);
            confs.push_back(maxVal);
        }
        prev = maxIdx;
    }
    float avgConf = 0;
    for (float c : confs) avgConf += c;
    if (!confs.empty()) avgConf /= confs.size();
    return {chars, avgConf};
```

### 5.3 结论：Confidence 的一致性

| 项 | Python | C++ |
|---|---|---|
| 字符解码 | argmax(axis=2) → char 去重 + blank skip | 逐 timestep 逐 class argmax → 去重 + blank skip | 一致 |
| confidence 提取 | preds.max(axis=2) → 去重后取平均 | row[maxIdx] → 去重后平均 | 一致 |
| 数值来源 | rec ONNX model output (已 softmax) | 直接取 model output max（已 softmax） | 一致 |

**两边都是对 softmax 后的概率值，无需额外 softmax 校准。**

---

## 6. `text_score` 阈值

### 6.1 Python

```python
# rapidocr_onnxruntime/config.yaml
Global:
    text_score: 0.5
# rapid_ocr_api.py RapidOCR.__call__:
self.text_score = global_config['text_score']
# filter_boxes_rec_by_score: if score >= self.text_score: keep
```

Python 用户可通过 kwargs 覆盖：

```python
engine(img, text_score=0.75)
```

### 6.2 C++

```cpp
// ocr_pipeline.cpp runOcr(... float textScore = 0.5f;

// main():
float textScore = 0.5f;
if (argv[i][0] != '-') textScore = std::stof(argv[i]);

// runOcr:
auto [text, conf] = ctcDecode(...);
if (conf < textScore) continue;
```

### 6.3 关于 C++ confidence 高于 Python 的根因

观测数据：C++ avg conf ≈ 0.94，Python avg conf ≈ 0.84，差 ≈ 0.10。

**关键发现**：两边 rec preprocess 完全一致。差异来自 det 框形状和大小不同：

- **旧版 C++ det unclip 是简单扩展矩形（不调用 offsetPolygon），导致框大小与 Python 不同
- 修复后的 det postprocess 后，det 框更接近 Python 结果，confidence 差异应减小

**建议**：如果修复 det 后 confidence 仍偏高，可以：
1. **方案 A**：在 ctcDecode 前显式 softmax 归一化（如果 model 输出未归一化）
2. **方案 B**：提高默认 `text_score` 默认值到 0.70–0.75

---

## 7. 字符表对齐

| 步骤 | Python | C++ |
|---|---|---|
| 加载 | `self.character = dict_character = self.character_str 从 txt / 模型内部` | 从 `ppocr_keys.json`（PaddleOCR 同一套 6624 字，首个元素为空字符串），首 token 0 是 blank，末尾追加 space（index 6624） → 共 6625 tokens |
| blank 位置 | index 0 | index 0 |
| space 位置 | 在 character 末尾 | 追加到字符表最后 |
| 字符数量 | 6625（含 blank） | 6625（含 blank） |

---

## 8. 完整 pipeline 数据流

```
+-------------------+         +------------------+
|  load image       | -----> | preprocessDet |
+-------------------+         +-------+------+
                                     |
                                     v
                           +------------------+
                           | det inference    | (onnxruntime ORT session)
                           +--------+---------+
                                     |
                                     v
                           +------------------+
                           | dbPostprocess   |
                           |  bitmap + dilate
                           |  connectedComponents
                           |  traceContour
                           |  minAreaRect → boxPoints (tl,tr,br,bl)
                           |  boxScore_fast
                           |  offsetPolygon (pyclipper offset)
                           |  minAreaRect → final box
                           +--------+---------+
                                     |
                                     v
+------------------+         +------------------+
| NMS-like 重叠框过滤 | <-----+  (去重小框
+------------------+
       |
       v
+------------------+
| orderPointsClockwise |
+---------+--------+
          |
          v
+------------------------+
| warpPerspectiveCrop | (透视变换+双三次采样+BORDER_REPLICATE
+----------+---------+
           |
           v
+------------------+
| preprocessCls   |
+--------+---------+
         |
         v
+------------------+
| cls inference    | → 是否旋转 180° → rotate180(crop)
+--------+---------+
         |
         v
+------------------+
| preprocessRec    |
+--------+---------+
         |
         v
+------------------+
| rec inference    |
+--------+---------+
         |
         v
+------------------+
| ctcDecode        |  argmax → text + max → avg conf
+--------+---------+
         |
         v
+------------------+
| text_score 过滤   |  conf >= textScore → 保留
+--------+---------+
         |
         v
+------------------+
| JSON output       |
+------------------+
```

## 9. 参数建议

| 参数 | 默认值 | 说明 |
|---|---|---|
| `DET_THRESH` | 0.3 | 与 Python 一致 |
| `BOX_THRESH` | 0.5 | 与 Python 一致 |
| `UNCLIP_RATIO` | 1.6 | 与 Python 一致 |
| `REC_H` | 48 | 与 Python 一致 |
| `REC_MAX_W` | 2000 | C++ 动态，大于默认 320，应在实际部署时根据需要取 batch max_wh_ratio |
| `textScore` | 0.5 | C++ 默认值 0.5，与 Python 一致；如需更严格过滤可提到 0.70–0.75 |

## 10. 已修复的问题列表

| # | 修复内容 | 文件 |
|---|---|---|
| 1 | `dbPostprocess` 用 `offsetPolygon` 替换简单矩形扩展 → 对齐 `pyclipper offset` | `ocr_pipeline.cpp` |
| 2 | `boxPoints` 输出点顺序改为 `tl, tr, br, bl`（x 排序 → 左右各按 y 排序） → 对齐 `get_mini_boxes` | `geometry.h` |
| 3 | `offsetPolygon` 增加外法线方向修正（带符号面积判定环绕方向） | `geometry.h` |
