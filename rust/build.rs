//! 构建期检查：`tts-cuda` 打开时必须真的能链到 GPU 版 sherpa-onnx。
//!
//! 为什么放在这里：运行期**无法**可靠判断 sherpa-onnx 到底用了哪个执行
//! 提供者——CPU 版库收到 `provider = "cuda"` 时只会打印一句
//! `Please compile with -DSHERPA_ONNX_ENABLE_GPU=ON ... Fallback to cpu!`
//! 然后**返回成功**。如果构建期不把关，界面就会把"CPU 在跑"显示成 CUDA。
//!
//! 这条检查保证：启用 `tts-cuda` 的二进制，链接的运行时里确实带了
//! `onnxruntime_providers_cuda.dll`（GPU 版 onnxruntime）。

use std::path::Path;

fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile ui/app.slint");
    check_tts_cuda();
}

fn check_tts_cuda() {
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TTS_CUDA");
    if std::env::var_os("CARGO_FEATURE_TTS_CUDA").is_none() {
        return;
    }
    let hint = "\
本地 TTS 的 CUDA 构建（--features tts-cuda）需要 GPU 版 sherpa-onnx 运行时：\n\
  1. 下载 sherpa-onnx-v2.0.1-cuda-13.x-cudnn-9.x-onnxruntime1.28.2-win-x64-cuda.tar.bz2\n\
  2. 解压后把 SHERPA_ONNX_LIB_DIR 指向其中的 lib 目录\n\
  3. Cargo.toml 里 sherpa-onnx 改成 default-features = false, features = [\"shared\"]\n\
  4. 运行时需要 cublas64_13 / cublasLt64_13 / cudart64_13 / cudnn64_9 等 DLL 在 PATH 里\n\
详见 rust/README.md「本地 TTS 用 CUDA」。";
    let dir = match std::env::var("SHERPA_ONNX_LIB_DIR") {
        Ok(dir) => dir,
        Err(_) => panic!("{hint}"),
    };
    if !Path::new(&dir)
        .join("onnxruntime_providers_cuda.dll")
        .is_file()
    {
        panic!(
            "SHERPA_ONNX_LIB_DIR={dir} 里没有 onnxruntime_providers_cuda.dll，\
             说明它不是 GPU 版运行时，CUDA 本地 TTS 不可能生效。\n{hint}"
        );
    }
}
