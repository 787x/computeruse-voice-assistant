"""
GSV-TTS-Lite 性能探针 v2：搞清楚 infer 到底是同步还是排队，并量出真实 RTF。

v1 的发现：infer() 每次都在 1.3 秒左右返回、且拿不到音频数据 —— 说明它把活派给了后台线程/队列，
wall time 不等于合成时间。要量真实性能只能看 infer_stream 的产出节奏。
"""
import json
import os
import sys
import time

os.environ.setdefault("PYTHONIOENCODING", "utf-8")

VOICE_DIR = r".runtime/voices/v2ProPlus/银狼"
REF_DIR = os.path.join(VOICE_DIR, "reference_audios", "中文", "emotions")
REF_WAV = os.path.join(REF_DIR, "【中立】前面是空间站的核心区域，反物质军团的虚卒会很多喔。.wav")
REF_TEXT = "前面是空间站的核心区域，反物质军团的虚卒会很多喔。"

SENTENCES = [
    "好的。",
    "我已经把浏览器打开了，接下来要搜什么？",
    "上海今天多云转晴，气温大概十七到二十一度，早晚有点凉，出门套件薄外套就够了。",
]


def audio_len(chunk):
    # AudioClip: audio_data(ndarray) / audio_len_s / samplerate
    return getattr(chunk, "audio_len_s", 0.0) or 0.0


def rate_of(chunk):
    value = getattr(chunk, "samplerate", None)
    return int(value) if isinstance(value, (int, float)) and value > 0 else None


def main():
    out = {}
    from gsv_tts import TTS
    import torch

    t0 = time.perf_counter()
    tts = TTS(use_bert=True)
    out["ctor_s"] = round(time.perf_counter() - t0, 2)   # 第二次跑：如果没有下载，应该快很多

    t0 = time.perf_counter()
    tts.load_gpt_model(os.path.join(VOICE_DIR, "银狼-e10.ckpt"))
    tts.load_sovits_model(os.path.join(VOICE_DIR, "银狼_e10_s120.pth"))
    out["load_models_s"] = round(time.perf_counter() - t0, 2)

    tts.cache_spk_audio(REF_WAV)
    tts.cache_prompt_audio(REF_WAV, REF_TEXT, prompt_language="zh")

    if torch.cuda.is_available():
        torch.cuda.reset_peak_memory_stats()

    runs = []
    for text in SENTENCES:
        started = time.perf_counter()
        first_at = None
        total_audio_s = 0.0
        rate = None
        chunks = 0
        for chunk in tts.infer_stream(
            spk_audio_path=REF_WAV,
            prompt_audio_path=REF_WAV,
            prompt_audio_text=REF_TEXT,
            text=text,
            text_language="zh",
            prompt_language="zh",
            stream_chunk=25,
            overlap_len=5,
        ):
            if first_at is None:
                first_at = time.perf_counter() - started
            total_audio_s += audio_len(chunk)
            rate = rate_of(chunk) or rate
            chunks += 1
        wall = time.perf_counter() - started
        audio_s = total_audio_s if total_audio_s > 0 else None
        runs.append({
            "chars": len(text),
            "ttft_s": round(first_at, 3) if first_at else None,
            "wall_s": round(wall, 3),
            "chunks": chunks,
            "audio_s": round(audio_s, 3) if audio_s else None,
            "rtf": round(wall / audio_s, 3) if audio_s else None,
            "rate": rate,
        })
    out["stream"] = runs

    if torch.cuda.is_available():
        out["vram_peak_mb"] = round(torch.cuda.max_memory_allocated() / 1048576, 1)
    print("__GSVPROBE2__" + json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        import traceback
        traceback.print_exc()
        print("__GSVPROBE2__" + json.dumps({"failed": f"{type(error).__name__}: {error}"}, ensure_ascii=False))
        sys.exit(1)
