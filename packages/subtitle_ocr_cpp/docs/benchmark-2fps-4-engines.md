# OCR Pipeline Benchmark — 4 Engines @ 2fps, subtitle-only, textScore=0.45

- Video: 170.1s internal reference video
- Frame extraction: `select='not(mod(n,step))'` at 2fps → **341 frames**
- Configuration: `subtitle_only=true, text_score_threshold=0.45`
- GT: `packages/benchmark/ref/metadata/ocr_manual.json` (75 segments, 559 normalized chars)
- Date: 2025-06-23

## Summary

| Engine | Segments | Norm CER | OCR inf (s) | RTF | avg/frame | det/frame | post/frame | rec/frame | hyp_chars |
|--------|---------:|---------:|------------:|-----:|----------:|----------:|-----------:|----------:|----------:|
| node (onnxruntime-node) | 75 | 0.18% | 219.9 | 1.301 | 645ms | — | — | — | 558 |
| python (rapidocr-onnxruntime) | 74 | 0.36% | 201.3 | 1.191 | 590ms | — | — | — | 558 |
| cpp-opencv | 76 | 0.89% | 121.1 | 0.717 | 355ms | 328ms | 11ms | 8ms | 560 |
| cpp (hand-written postprocess) | 76 | 1.25% | 123.0 | 0.728 | 361ms | — | — | — | 557 |

Notes:
- `python` missed 1 GT segment — the filler word "啊". Matched 74/75.
- `node` matched 75/75 segments with identical text output (558 chars, only 1 char difference from GT 559 after normalization).
- Both C++ pipelines have more segments than GT (76 vs 75) and a few extra characters, hence higher CER. The gap vs python is ~0.5-0.9ppt in absolute terms.
- C++ pipelines are ~1.7× faster than python/node (dominated by ORT CPU session init overhead per frame in python/node, not by preprocessing).

## Per-frame timing breakdown (cpp-opencv)

```
Total OCR per frame:          355ms
├─ det inference (ORT):       328ms   (92%)
├─ det postprocess (cv):       11ms   ( 3%)
├─ rec inference (ORT):         7.6ms ( 2%)
└─ preprocess + crop:           8.4ms ( 2%)
```

The hand-written `cpp` pipeline has near-identical timings (361ms vs 355ms); the OpenCV postprocessing does not change the overall cost materially.

## Remaining opportunities in cpp-opencv

The C++ pipeline still has hand-written image processing in three places (from `ocr_pipeline.cpp` in `ort-cpp/`):

| Step | Current | Can replace with `cv::` | Expected speedup per call |
|------|---------|------------------------|---------------------------|
| `preprocessDet` | `resizeBilinear` (nested loops) | `cv::Mat + cv::resize(INTER_LINEAR)` + `cv::normalize`/manual | 3-5× (biggest single piece) |
| `preprocessCls` | `resizeBilinear` + manual zero-pad | `cv::resize` + `cv::copyMakeBorder` | 2-4× |
| `preprocessRec` | `resizeBilinear` + manual zero-pad | same as cls | 2-4× |
| `warpPerspectiveCrop` | Gaussian-elimination 8×9 + `sampleBicubic` | `cv::getPerspectiveTransform` + `cv::warpPerspective(INTER_LINEAR)` | 3-8× per detected box |
| `rotate180` | nested-loop pixel swap | `cv::flip(src, dst, -1)` | small (already fast) |

However, because **preprocessing + warping are only ~8ms per frame** (vs 328ms for det inference), even a 5× improvement there would only shave ~6ms per frame (~1.7% of total). So the real value of switching to `cv::` is not speed but **numeric consistency with the Python rapidocr pipeline**, which would likely also improve CER slightly.

## Key takeaways

1. **2fps textScore=0.45 is already very good** — all engines produce CER < 1.3%.
2. **node/python pipeline has matching preprocessing** (both use `cv2.resize(INTER_LINEAR)` internally), giving near-zero char-level differences.
3. **C++ pipeline uses hand-written `resizeBilinear`** — this may cause small numeric differences (different rounding at boundaries, different interpolation weight for edge pixels) and is the most likely source of the extra segments/characters in C++ output.
4. **Replacement priority**:
   - P1: `preprocessDet` — runs once per frame on the largest image, most likely to affect det heatmap → seg count → CER.
   - P2: `warpPerspectiveCrop` — runs once per detected box, affects rec quality, and the current Gaussian-elimination solver is both slower and potentially numerically different from `cv::getPerspectiveTransform`.
   - P3: `preprocessCls` / `preprocessRec` — small images, less sensitive.
   - P4: `rotate180` — trivial, negligible impact.
