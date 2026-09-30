# 仓库根入口：跨包 / 跨目录的杂项脚本。
# 用法（在仓库根下）：
#   just                          # 默认 = 显示任务列表
#   just rename-strip <dir>       # 去掉目录下文件名的前缀（默认 frame_），对接 subtitle_ocr 的 --dir 时间命名
#   just rename-strip <dir> --prefix shot_   # 去掉其他前缀（如 shot_）
#   just rename-strip <dir> --dry-run         # 只预览、不改名
#
# 说明：subtitle_ocr 的 --dir 要求文件名本身即时间数值（ms / ms_ms），不允许 frame_ 等
# 语义前缀，故抽帧产出的 frame_0000030.jpg 需先改名为 0000030.jpg 才能被识别。

# ---- 默认任务（`just` 无参即显示列表） ----
default:
    @just --list

# ---- 批量去掉文件名前缀（默认 frame_），对接 subtitle_ocr --dir 命名约定 ----
# 用法：just rename-strip <dir> [extra-args...]
#   extra-args 透传给脚本：--prefix <p> / --dry-run
rename-strip dir extra_args="":
    node scripts/rename_strip_prefix.mjs {{dir}} {{extra_args}}

# ---- 统合后处理管线：一条命令串起 adjust-box → filter-box → merge → adjust-segment → filter-segment ----
# 用法：just ocr-post [frames] [out] [video_height] [threshold] [stop_at]
# 输入 frames（逐帧 OCR JSON，subtitle_ocr --out 产出）；画面高度取 frames 的
# meta.video_height（识别侧写入），老 JSON 没有该字段时用 video_height 显式给。
# 各中间产物（frames_box_adjust.json / frames_box_filter.json / frames_merged.json /
# segment_adjust.json / segment_filter.json）都写到 out 目录。--stop-at 可只跑到某一步。
# 二进制 = subtitle_ocr_post（src/main.rs，默认 bin，无需 --bin）
subtitle_ocr_post frames="workfolder/师尊带我炸修真/8/sf_ocr/frames.json" out="workfolder/师尊带我炸修真/8/sf_ocr_fix"  threshold="0.45" stop_at="filter-segment":
    cargo run -p subtitle_ocr_post --release -- --frames {{frames}} --out {{out}} --threshold {{threshold}} --stop-at {{stop_at}}
