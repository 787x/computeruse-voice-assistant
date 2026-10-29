"""
GSV-TTS-Lite 性能探针（银狼 v2ProPlus）。

只回答三个问题：
  1. Blackwell(sm_120) 能不能跑起来（Lite 用了 CUDA Graph）
  2. 首包延迟 / RTF 是多少
  3. 显存占多少（8GB 还要给 MuMu 留）

用法：.runtime/venv/Scripts/python.exe tools/probe-gsvsovits.py
"""
import json
import os
import sys
import time
import wave

os.environ.setdefault("PYTHONIOENCODING", "utf-8")

VOICE_DIR = r".runtime/voices/v2ProPlus/银狼"
GPT_MODEL = os.path.join(VOICE_DIR, "银狼-e10.ckpt")
SOVITS_MODEL = os.path.join(VOICE_DIR, "银狼_e10_s120.pth")
REF_DIR = os.path.join(VOICE_DIR, "reference_audios", "中文", "emotions")
REF_WAV = os.path.join(REF_DIR, "【中立】前面是空间站的核心区域，反物质军团的虚卒会很多喔。.wav")
REF_TEXT = "前面是空间站的核心区域，反物质军团的虚卒会很多喔。"

SENTENCES = [
    "好的。",                                              # 极短
    "我已经把浏览器打开了，接下来要搜什么？",                  # 中等（约 20 字）
    "上海今天多云转晴，气温大概十七到二十一度，早晚有点凉，出门套件薄外套就够了。",  # 长（约 40 字）
]


def wav_seconds(path):
    with wave.open(path) as w:
        return w.getnframes() / w.getframerate()


def audio_seconds(audio):
    """Lite 返回的对象字段名可能变，逐个试，拿不到就返回 None（不影响其它指标）"""
    for data_attr, rate_attr in (("data", "sample_rate"), ("audio", "sample_rate"), ("samples", "sr")):
        data = getattr(audio, data_attr, None)
        rate = getattr(audio, rate_attr, None)
        if data is not None and rate:
            try:
                return len(data) / rate
            except TypeError:
                continue
    return None


def main():
    report = {"stage": "init"}
    t0 = time.perf_counter()
    from gsv_tts import TTS
    report["import_s"] = round(time.perf_counter() - t0, 2)

    t0 = time.perf_counter()
    tts = TTS(use_bert=True)
    report["ctor_s"] = round(time.perf_counter() - t0, 2)

    report["stage"] = "load_models"
    t0 = time.perf_counter()
    tts.load_gpt_model(GPT_MODEL)
    tts.load_sovits_model(SOVITS_MODEL)
    report["load_models_s"] = round(time.perf_counter() - t0, 2)

    report["stage"] = "cache_ref"
    t0 = time.perf_counter()
    tts.cache_spk_audio(REF_WAV)
    tts.cache_prompt_audio(REF_WAV, REF_TEXT, prompt_language="zh")
    report["cache_ref_s"] = round(time.perf_counter() - t0, 2)

    import torch
    if torch.cuda.is_available():
        torch.cuda.reset_peak_memory_stats()

    report["stage"] = "infer"
    runs = []
    for text in SENTENCES:
        started = time.perf_counter()
        audio = tts.infer(
            spk_audio_path=REF_WAV,
            prompt_audio_path=REF_WAV,
            prompt_audio_text=REF_TEXT,
            text=text,
            text_language="zh",
            prompt_language="zh",
        )
        elapsed = time.perf_counter() - started
        seconds = audio_seconds(audio)
        runs.append({
            "text": text,
            "chars": len(text),
            "wall_s": round(elapsed, 3),
            "audio_s": round(seconds, 3) if seconds else None,
            "rtf": round(elapsed / seconds, 3) if seconds else None,
        })
    report["infer"] = runs

    # 流式首包：用最长那句
    report["stage"] = "stream"
    try:
        started = time.perf_counter()
        first_at = None
        chunks = 0
        for chunk in tts.infer_stream(
            spk_audio_path=REF_WAV,
            prompt_audio_path=REF_WAV,
            prompt_audio_text=REF_TEXT,
            text=SENTENCES[2],
            text_language="zh",
            prompt_language="zh",
            stream_chunk=25,
            overlap_len=5,
        ):
            if first_at is None:
                first_at = time.perf_counter() - started
            chunks += 1
        report["stream"] = {
            "first_chunk_s": round(first_at, 3) if first_at else None,
            "chunks": chunks,
            "total_s": round(time.perf_counter() - started, 3),
        }
    except Exception as error:  # 流式失败不该让整份报告作废
        report["stream_error"] = f"{type(error).__name__}: {error}"

    if torch.cuda.is_available():
        report["vram"] = {
            "peak_allocated_mb": round(torch.cuda.max_memory_allocated() / 1048576, 1),
            "peak_reserved_mb": round(torch.cuda.max_memory_reserved() / 1048576, 1),
        }
    report["stage"] = "done"
    print("__GSVPROBE__" + json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        import traceback
        traceback.print_exc()
        print("__GSVPROBE__" + json.dumps({"stage": "failed", "error": f"{type(error).__name__}: {error}"}, ensure_ascii=False))
        sys.exit(1)
