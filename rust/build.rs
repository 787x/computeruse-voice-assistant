//! 构建期检查：`tts-cuda` 打开时必须真的能链到 GPU 版 sherpa-onnx。
//!
//! 为什么放在这里：运行期**无法**可靠判断 sherpa-onnx 到底用了哪个执行
//! 提供者——CPU 版库收到 `provider = "cuda"` 时只会打印一句
//! `Please compile with -DSHERPA_ONNX_ENABLE_GPU=ON ... Fallback to cpu!`
//! 然后**返回成功**。如果构建期不把关，界面就会把"CPU 在跑"显示成 CUDA。
//!
//! 链接方式由本 crate 的 feature 决定（不再需要手改 `Cargo.toml`）：
//!  - 默认（不启用 `tts-static` / `tts-cuda`）：sherpa-onnx-sys 自己按静态库链接；
//!  - `--features tts-cuda`：`sherpa-onnx/shared`，链接 GPU 版运行时；
//!  - `--features tts-static`：显式要求静态库（与 `tts-cuda` 互斥）。
//!
//! **注意**：`tts-cuda` 只解决"链接对 GPU 运行时"。它**不**证明本机真的能用
//! CUDA 合成——那还需要 cuDNN 9 与可用显存，运行期由 `LocalTts` 如实回报。

use std::path::Path;

fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile ui/app.slint");
    check_sherpa_link_mode();
    check_tts_cuda();
}

/// `tts-static` 与 `tts-cuda` 不能同时开：sherpa-onnx-sys 会直接报错，
/// 这里先给出可读的中文原因，避免用户面对一句英文的构建脚本 panic。
fn check_sherpa_link_mode() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TTS_STATIC");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TTS_CUDA");
    let static_on = std::env::var_os("CARGO_FEATURE_TTS_STATIC").is_some();
    let cuda_on = std::env::var_os("CARGO_FEATURE_TTS_CUDA").is_some();
    if static_on && cuda_on {
        panic!(
            "`tts-static` 与 `tts-cuda` 互斥：本地 TTS 只能选择一种 sherpa-onnx 链接方式。\n\
             启用 GPU 版请只用 `--features tts-cuda`（默认构建已经是静态 CPU 版）。"
        );
    }
}

fn check_tts_cuda() {
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    if std::env::var_os("CARGO_FEATURE_TTS_CUDA").is_none() {
        return;
    }
    let hint = "\
本地 TTS 的 CUDA 构建（--features tts-cuda）需要 GPU 版 sherpa-onnx 运行时：\n\
  1. 下载 sherpa-onnx-v2.0.1-cuda-13.x-cudnn-9.x-onnxruntime1.28.2-win-x64-cuda.tar.bz2\n\
     （CUDA 12 的机器用 cuda-12.x 那个包）\n\
  2. 解压后把 SHERPA_ONNX_LIB_DIR 指向其中的 lib 目录\n\
  3. 运行时需要 cublas64_* / cublasLt64_* / cudart64_* 与 cuDNN 9 的 cudnn64_9.dll 在 PATH 或 exe 同目录\n\
  4. 重新执行 cargo build --release --features tts-cuda\n\
详见 rust/README.md「本地 TTS 用 CUDA（实验性）」。
注意：这只保证链接了 GPU 运行时，不保证本机装有 cuDNN 9 或显存可用。";
    let dir = match std::env::var("SHERPA_ONNX_LIB_DIR") {
        Ok(dir) => dir,
        Err(_) => panic!(
            "启用 tts-cuda 但没有设置 SHERPA_ONNX_LIB_DIR：{}",
            hint.trim_end()
        ),
    };
    let lib = Path::new(&dir);
    if !lib.is_dir() {
        panic!("SHERPA_ONNX_LIB_DIR={dir} 不是一个目录。\n{hint}");
    }
    // GPU 版运行时的判据：CUDA execution provider 的 DLL 必须在。
    // 只看"目录里有 DLL"是不够的——shared CPU 包同样有 DLL。
    if !lib.join("onnxruntime_providers_cuda.dll").is_file() {
        panic!(
            "SHERPA_ONNX_LIB_DIR={dir} 里没有 onnxruntime_providers_cuda.dll，\
             说明它不是 GPU 版运行时，CUDA 本地 TTS 不可能生效。\n{hint}"
        );
    }
    // 链接还需要 C API 的导入库；缺了会在链接阶段报一堆难以理解的符号错误。
    if !lib.join("sherpa-onnx-c-api.lib").is_file() {
        panic!(
            "SHERPA_ONNX_LIB_DIR={dir} 里没有 sherpa-onnx-c-api.lib（shared 链接需要导入库）。\n{hint}"
        );
    }
}
