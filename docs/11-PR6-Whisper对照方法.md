# whisper.cpp / faster-whisper 短指令对照方法

这是**开发期实验**，不是产品的一部分：生产构建、启动、测试主路径都不依赖
Python 或 faster-whisper。没有 Python 环境时就如实报告「未执行」。

## 1. 目的

回答一个具体问题：**端点之后多久能拿到文本**。
所以只测「1~5 秒的短指令」，不测长音频批处理吞吐——后者和本产品的响应速度无关。

## 2. 同一组输入

| 项 | 取值 |
| --- | --- |
| 音频 | `assets/test/asr-zh.wav`（3.77 s 中文指令），可再加同目录的短句 |
| 语言 | `zh`（两边都显式指定，不依赖自动检测） |
| 解码 | 贪心：whisper.cpp `SamplingStrategy::Greedy`；faster-whisper `beam_size=1` |
| VAD | **两边都关闭**，直接喂同一段已切好的音频（绕开切分差异） |
| 预热 | 先跑一次丢弃，再测 `--repeats` 次 |

## 3. 指标

- **模型加载**：冷启动，单独计时（这决定「唤醒后第一句」的体验）。
- **端点后转录延迟**：p50 / p95（毫秒）。
- **RTF**：解码耗时 ÷ 音频时长。
- **识别质量**：把文本原样打印出来人工比对，不做自动打分
  （自动打分要引入另一个模型，误差反而更大）。
- **RAM/VRAM**：只用系统工具粗测；取不到就写 `null`，不编数字。

## 4. 怎么跑

```powershell
# 1) whisper.cpp 侧（同一台机器、同一模型）
cd rust
cargo build --release --example whisper_bench
# CPU-only 版：cargo build --release --no-default-features --example whisper_bench
$env:CUAVA_WHISPER_MODEL = "D:\path\to\ggml-small.bin"
.\target\release\examples\whisper_bench.exe ..\assets\test\asr-zh.wav 8 5

# 2) 两边一起对照
cd ..
node tools/bench-whisper.mjs --wavs assets/test/asr-zh.wav `
  --cpp-bin rust/target/release/examples/whisper_bench.exe `
  --model .tmp/whisper-models/ggml-small.bin `
  --fw-model small --language zh --threads 8 --repeats 5
```

## 5. 已知不可比之处（写进结论里，别假装对齐）

- **权重格式不同**：whisper.cpp 用 ggml，faster-whisper 用 CTranslate2；
  `int8` 不是同一种量化，必须标注。
- **线程模型不同**：CTranslate2 内部有自己的线程池，`threads` 语义不完全等价。
- **GPU 后端不同**：cuBLAS vs CTranslate2 的 CUDA kernel，算子实现不同。

所以对照只能在同一台机器、同一组样本上做**方向性**比较，
不要把某一个机型的数字外推成「谁更快」的结论。

## 6. 本机已记录的结果（供后续对照）

Ryzen 9 9955HX + RTX 5070 Laptop，`ggml-small`，`asr-zh.wav` 3.77 s：

| 后端 | 模型加载 | 转录 p50 | RTF | 文本 |
| --- | --- | --- | --- | --- |
| CPU 8 线程 | ≈ 0.35 s | 2335 ms | 0.62 | 帮我把浏览器倒开,然后搜索今天的天气。 |
| CUDA | ≈ 0.66 s | 155 ms | 0.041 | 同上 |

faster-whisper 侧**未执行**：本机当时未安装 `faster_whisper`（CTranslate2）。
按上面的命令跑完把结果补进这张表即可；在此之前不要声称已做过对比。
