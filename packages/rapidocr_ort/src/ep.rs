//! 推理执行后端（Execution Provider）选择：CPU / CUDA / DirectML。
//!
//! ## 设计：编译期 feature × 运行时选择
//!
//! ONNX Runtime 的 EP 支持是**编译进二进制**的（ort crate 的 `cuda` / `directml`
//! feature 决定 ort-sys 拉取哪个预编译库），而用哪个后端是**运行时**决定的
//! （各 CLI 的 `--ep`）。本 crate 暴露两个 feature 把两者接起来：
//!
//! - `ep-cuda` = `ort/cuda`：预编译 ORT 带 CUDA EP；运行时还需 CUDA/cuDNN 的
//!   DLL 在 `PATH`（cudart / cublas / cufft / curand / cudnn，可经 pip 的
//!   `nvidia-*-cu12` 轮子获取）。
//! - `ep-directml` = `ort/directml`：Windows 10+ 系统自带 DirectML，无需额外 DLL。
//!
//! 默认（无任何 `ep-*` feature）只支持 CPU。
//!
//! ## 失败语义：宁可硬失败，不静默回退
//!
//! - `--ep cuda` 跑在**没编译** CUDA 支持的二进制上 → 硬错误（而非静默回退
//!   CPU），否则基准测试会出现「以为在测 GPU、实际是 CPU」的假数据。
//! - `--ep cuda` 跑在**编译了** CUDA 但运行时缺 DLL 的机器上 → ORT 初始化 EP
//!   失败时会打 warning 并回退 CPU 执行（这是 ORT 层行为，拦截不了）；因此
//!   基准/冒烟时必须核对进程能报出实际使用的 EP（或核对 GPU 占用/耗时量级）。

use anyhow::Result;
use clap::ValueEnum;
use ort::session::builder::SessionBuilder;

/// 推理执行后端。各 CLI `--ep` 的取值，默认 CPU。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, ValueEnum)]
pub enum ExecutionBackend {
    /// CPU（默认；ONNX Runtime 内置，无需额外 feature）。
    #[default]
    Cpu,
    /// NVIDIA CUDA（需 `ep-cuda` feature 构建 + 运行时 CUDA/cuDNN DLL）。
    Cuda,
    /// DirectML（需 `ep-directml` feature 构建；Windows 10+ 无额外依赖）。
    // clap 默认把变体名 kebab 化成 `direct-ml`；对外统一 `directml`。
    #[value(name = "directml")]
    DirectML,
}

impl ExecutionBackend {
    /// 后端名的稳定小写字符串（写进输出 meta / 基准 label，区分 CPU/GPU 结果）。
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionBackend::Cpu => "cpu",
            ExecutionBackend::Cuda => "cuda",
            ExecutionBackend::DirectML => "directml",
        }
    }

    /// 把后端应用到 session builder；返回的 builder 随后 `commit_from_file` 加载模型。
    ///
    /// 三个 session（det/rec/cls）都要走同一个后端，保持各阶段设备一致。
    ///
    /// GPU 设备序号经环境变量 `ORT_EP_DEVICE_ID` 指定（默认 0）。多适配器机器
    /// （核显 + 独显 + 虚拟显示适配器）上 0 未必是独显——设备枚举顺序由
    /// DirectX/DirectML 决定，可用 `nvidia-smi -q | grep -i "device index"` 之外
    /// 的实测（如对比两档耗时）确认 0 落在哪个适配器上，再显式覆盖。
    pub(crate) fn apply(self, builder: SessionBuilder) -> Result<SessionBuilder> {
        let device_id: i32 = std::env::var("ORT_EP_DEVICE_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match self {
            ExecutionBackend::Cpu => {
                let _ = device_id;
                Ok(builder)
            }
            ExecutionBackend::Cuda => {
                #[cfg(feature = "ep-cuda")]
                {
                    builder
                        .with_execution_providers([ort::ep::CUDA::default()
                            .with_device_id(device_id)
                            .build()])
                        .map_err(|e| anyhow::anyhow!("注册 CUDA 执行后端失败: {e}"))
                }
                #[cfg(not(feature = "ep-cuda"))]
                {
                    let _ = builder;
                    let _ = device_id;
                    anyhow::bail!(
                        "CUDA 执行后端需以 `ep-cuda` feature 构建的二进制 \
                         （cargo build --release -p subtitle_ocr --features ep-cuda），\
                         当前二进制仅支持 CPU"
                    )
                }
            }
            ExecutionBackend::DirectML => {
                #[cfg(feature = "ep-directml")]
                {
                    builder
                        .with_execution_providers([ort::ep::DirectML::default()
                            .with_device_id(device_id)
                            .build()])
                        .map_err(|e| anyhow::anyhow!("注册 DirectML 执行后端失败: {e}"))
                }
                #[cfg(not(feature = "ep-directml"))]
                {
                    let _ = builder;
                    let _ = device_id;
                    anyhow::bail!(
                        "DirectML 执行后端需以 `ep-directml` feature 构建的二进制 \
                         （cargo build --release -p subtitle_ocr --features ep-directml），\
                         当前二进制仅支持 CPU"
                    )
                }
            }
        }
    }
}
