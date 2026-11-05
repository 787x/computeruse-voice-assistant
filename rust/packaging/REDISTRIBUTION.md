# 再分发清单与审核边界

- Slint 1.18.1：GPL-3.0-only / Royalty-free / 商业许可的多许可选择。发布者须确认所选条款；本候选未取得商业许可或宣称满足所有发布条款。
- whisper.cpp/ggml：MIT；sherpa-onnx：Apache-2.0；ONNX Runtime：MIT。本次静态库清单及 SHA-256 见 native/linked-archive-sha256.json；按 sherpa-onnx v2.0.1 构建文件核对 kaldi-native-fbank 1.22.4、kaldi-decoder 0.3.0、OpenFst 1.8.5-2026-07-09、simple-sentencepiece 0.7、utf8proc 2.12.0、KissFFT 固定提交和 ONNX Runtime 1.28.2。各许可证与 ONNX Runtime ThirdPartyNotices 已收集到 native/，来源及原文哈希见 sources.json。v2.0.1 上游已移除 espeak-ng/piper-phonemize，当前链接库列表也不包含这两个独立库；模型目录中的 espeak-ng-data 不随程序分发。
- Cargo 依赖：licenses/cargo-inventory.txt 记录锁定版本、SPDX 和仓库，随附包根目录可找到的 LICENSE/COPYING/NOTICE 和嵌套 LICENSES 目录，保留 Slint 的完整许可选项正文。
- MSVC CRT：仅允许从获许可的 Visual Studio Redist 目录复制，适用 Microsoft Visual Studio 再分发条款；不是从 System32 猜测复制。UCRT 为 Windows 10/11 系统组件。
- 字体：用系统字体，不打包字体。托盘图标为项目自行绘制的中性像素图，无第三方图标。
- NSIS：zlib/libpng 许可，工具仅用于生成安装器；所用插件/发行版本仍应审核原始许可。
- 模型/声音：不打包、不自动下载。KWS/Whisper/Kokoro/声音各自许可由用户选择来源时确认；不得将代码许可推定为权重许可。

已核对并补齐上述源码版本可取得的许可证/NOTICE；这不是对所有再分发条件均已满足的法律声明。Slint 所选许可条款及 MSVC 再分发授权仍需发布者在正式发布前确认，干净系统测试也尚未完成。继续作为发布阻断，不发布 Release。


核对依据：sherpa-onnx [v2.0.1 发布记录](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v2.0.1)及其 cmake 构建清单。ONNX 静态归档内能匹配到 1.28.2 版本字符串。预编译包原目录只含 lib，故补取与该版本构建依赖相对应的原作者通知，并保留实际链接归档哈希；不把 Cargo crate 的顶层许可误当为整个原生归档的许可。
