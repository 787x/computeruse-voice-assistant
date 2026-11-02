#!/usr/bin/env node
/**
 * whisper.cpp 与 faster-whisper 的**开发期**对照脚本（PR6 第 5 条）。
 *
 * 这不是产品的一部分：
 *   - 生产构建、启动、测试主路径都不依赖它；
 *   - 它需要本机装了 Python + faster_whisper（CTranslate2），且只在开发机上跑；
 *   - 缺环境时它会明确报告「未执行」，而不是给一个编出来的数字。
 *
 * 用法：
 *   node tools/bench-whisper.mjs \
 *     --wavs assets/test/asr-zh.wav[,其它...] \
 *     --model .tmp/whisper-models/ggml-small.bin \
 *     --cpp-bin D:/cuava/rust/target/release/whisper_bench.exe \
 *     --fw-model small --language zh --threads 8 [--repeats 3]
 *
 * 对照口径（必须一致，否则数字没有可比性）：
 *   - 同一组短句 WAV（16k 单声道最省事；不是的话两边都先重采样）；
 *   - 同一个模型规模（small ↔ small）、同一语言、同一种推理参数
 *     （faster-whisper: beam_size=1 / whisper.cpp: greedy）；
 *   - **冷启动**分别记录「模型加载」与「端点后转录延迟」；
 *   - 只测短指令（1~5 秒），不拿长音频批处理吞吐当响应速度。
 *
 * 已知不可比之处（脚本会在输出里标注）：
 *   - whisper.cpp 用 ggml 权重，faster-whisper 用 CTranslate2 权重，量化方式不同；
 *   - 两边的 VAD 切分不一致：这里两边都喂**同一段已切好的音频**，绕开这个问题；
 *   - RAM/VRAM 只能用系统工具粗测（脚本不伪造数字，取不到就写 null）。
 */
import { execFileSync } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'

function arg(name, fallback = null) {
  const i = process.argv.indexOf(`--${name}`)
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : fallback
}

const wavs = (arg('wavs') ?? 'assets/test/asr-zh.wav').split(',').filter(Boolean)
const cppBin = arg('cpp-bin')
const fwModel = arg('fw-model', 'small')
const language = arg('language', 'zh')
const threads = Number(arg('threads', '8'))
const repeats = Number(arg('repeats', '3'))
const format = arg('fw-format', 'int8') // int8 / float16 / float32

const report = {
  env: {
    platform: process.platform,
    node: process.version,
    language,
    threads,
    repeats,
    fwModel,
    fwFormat: format,
  },
  warnings: [],
  cases: [],
  verdict: {},
}

report.warnings.push(
  '两边的模型权重格式不同（ggml vs CTranslate2），量化不可完全对齐；标注了各自的格式。',
)

// ---------------------------------------------------------------- whisper.cpp

/**
 * whisper.cpp 侧通过一个**开发用**的小程序跑：
 * 交付物里不包含它（`cargo run --example` 也不要求），
 * 这里用 `tests/bench_whisper.rs` 编译出的可执行文件。
 */
function runCpp(wav) {
  if (!cppBin || !existsSync(cppBin)) {
    return { skipped: `找不到 whisper.cpp 基准程序：${cppBin ?? '(未指定 --cpp-bin)'}` }
  }
  try {
    const out = execFileSync(cppBin, [wav, String(threads), String(repeats)], {
      encoding: 'utf8',
      timeout: 600_000,
    })
    return JSON.parse(out)
  } catch (error) {
    return { error: error.message.slice(0, 400) }
  }
}

// ---------------------------------------------------------------- faster-whisper

/**
 * faster-whisper 侧通过一段内联 Python 跑。
 *
 * 为什么内联而不是放一个 .py 文件：这个脚本本身就不是交付物，
 * 内联能让「看了就知道测了什么」，也避免再维护一个实验用文件。
 * 使用 `-I`：不加载当前目录 / 用户脚本目录的代码。
 */
const PY = String.raw`
import json, sys, time, statistics, resource

wavs = json.loads(sys.argv[1])
model_name = sys.argv[2]
language = sys.argv[3] if sys.argv[3] != "" else None
compute_type = sys.argv[4]
repeats = int(sys.argv[5])

out = {"engine": "faster-whisper", "cases": [], "error": None}
try:
    from faster_whisper import WhisperModel
except Exception as e:
    out["error"] = "faster_whisper 未安装: %s" % e
    print(json.dumps(out))
    sys.exit(0)

t0 = time.perf_counter()
model = WhisperModel(model_name, device="auto", compute_type=compute_type)
load_ms = (time.perf_counter() - t0) * 1000
out["loadMs"] = round(load_ms, 1)

for wav in wavs:
    times = []
    text = ""
    audio_s = 0.0
    for _ in range(repeats):
        t = time.perf_counter()
        segments, info = model.transcribe(
            wav,
            language=language,
            beam_size=1,
            vad_filter=False,
            condition_on_previous_text=False,
        )
        pieces = []
        for seg in segments:
            pieces.append(seg.text)
        times.append((time.perf_counter() - t) * 1000)
        audio_s = info.duration
        text = "".join(pieces).strip()
    times.sort()
    out["cases"].append({
        "wav": wav,
        "audioSeconds": round(audio_s, 2),
        "decodeMs": [round(x, 1) for x in times],
        "p50Ms": round(statistics.median(times), 1),
        "p95Ms": round(times[int(len(times) * 0.95) - 1] if len(times) > 1 else times[0], 1),
        "rtf": round((statistics.median(times) / 1000.0) / audio_s, 3) if audio_s else None,
        "text": text,
    })

# 进程自身峰值内存（RSS），仅 Linux/macOS 准确；Windows 返回 0
try:
    out["peakRssMb"] = round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)
except Exception:
    out["peakRssMb"] = None

print(json.dumps(out))
`

function runFasterWhisper() {
  try {
    const out = execFileSync(
      'python',
      ['-I', '-c', PY, JSON.stringify(wavs), fwModel, language, format, String(repeats)],
      { encoding: 'utf8', timeout: 1_800_000 },
    )
    const parsed = JSON.parse(out.trim().split('\n').pop())
    return parsed
  } catch (error) {
    return { error: `无法运行 Python/faster-whisper：${error.message.slice(0, 300)}` }
  }
}

// ---------------------------------------------------------------- 汇总

const cpp = {}
for (const wav of wavs) cpp[wav] = runCpp(wav)
const fw = runFasterWhisper()

report.cpp = cpp
report.fasterWhisper = fw

if (report.fasterWhisper?.error) {
  report.warnings.push(`faster-whisper 对照**未执行**：${report.fasterWhisper.error}`)
}
const cppSkipped = Object.values(cpp).every((v) => v && v.skipped)
if (cppSkipped) {
  report.warnings.push('whisper.cpp 对照**未执行**：找不到基准程序（先用 cargo build --release --example 构建）')
}

report.verdict = {
  cppExecuted: !cppSkipped,
  fasterWhisperExecuted: !report.fasterWhisper?.error,
  // 不产出「谁更快」这种结论：样本太少，且差值与机型强相关。
  note: '结论只应在同一台机器、同一组样本上横向比较；不要外推到其它硬件。',
}

console.log(JSON.stringify(report, null, 2))
