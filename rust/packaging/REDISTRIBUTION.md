# 再分发清单与审核边界

- Slint 1.18.1：GPL-3.0-only / Royalty-free / 商业许可的多许可选择。发布者须确认所选条款；本候选未取得商业许可或宣称满足所有发布条款。
- whisper.cpp/ggml：MIT；sherpa-onnx：Apache-2.0；ONNX Runtime：MIT。静态库还包含 kaldi-native-fbank、kaldifst/OpenFst、espeak-ng、sentencepiece、kissfft、utf8proc 等上游组件，必须按所用预编译包原始 NOTICE/LICENSE 核对，Cargo 顶层许可证清单不代表这些归档已全部覆盖。
- Cargo 依赖：licenses/cargo-inventory.txt 记录锁定版本、SPDX 和仓库，随附包根目录可找到的 LICENSE/COPYING/NOTICE。发布前补齐原生归档内部通知。
- MSVC CRT：仅允许从获许可的 Visual Studio Redist 目录复制，适用 Microsoft Visual Studio 再分发条款；不是从 System32 猜测复制。UCRT 为 Windows 10/11 系统组件。
- 字体：用系统字体，不打包字体。托盘图标为项目自行绘制的中性像素图，无第三方图标。
- NSIS：zlib/libpng 许可，工具仅用于生成安装器；所用插件/发行版本仍应审核原始许可。
- 模型/声音：不打包、不自动下载。KWS/Whisper/Kokoro/声音各自许可由用户选择来源时确认；不得将代码许可推定为权重许可。

此清单公开尚待审核的原生预编译归档许可证边界；未完成审核与干净系统测试前，产物只作为内部发行候选，不发布 Release。
