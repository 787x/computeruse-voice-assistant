"""
生成试听样本：同一句话用不同情绪参考音频说，量出音色/风格解耦是否真的有效。

输出：.runtime/samples/*.wav（32kHz）
"""
import json
import os
import time

os.environ.setdefault("PYTHONIOENCODING", "utf-8")

VOICE_DIR = r".runtime/voices/v2ProPlus/银狼"
REF_DIR = os.path.join(VOICE_DIR, "reference_audios", "中文", "emotions")
OUT_DIR = r".runtime/samples"

# 情绪 → (参考音频文件名, 对应文本)
REFS = {
    "中立": ("【中立】前面是空间站的核心区域，反物质军团的虚卒会很多喔。.wav",
             "前面是空间站的核心区域，反物质军团的虚卒会很多喔。"),
    "生气": ("【生气】事先说清楚，这只是我的直觉，对你不一定管用，如果没有效果，我不负责。.wav",
             "事先说清楚，这只是我的直觉，对你不一定管用，如果没有效果，我不负责。"),
    "开心": ("【开心】虽然我们的立场不同…但在这样一场宇宙级的盛会上，就先放下那些成见，共同享受游戏的快乐吧——你说呢？.wav",
             "虽然我们的立场不同…但在这样一场宇宙级的盛会上，就先放下那些成见，共同享受游戏的快乐吧——你说呢？"),
}

LINES = [
    "好的，我把浏览器打开了，要搜什么？",
    "上海今天多云转晴，十七到二十一度，早晚有点凉。",
]


def main():
    from gsv_tts import TTS
    import torch

    os.makedirs(OUT_DIR, exist_ok=True)
    tts = TTS(use_bert=True)
    t0 = time.perf_counter()
    tts.load_gpt_model(os.path.join(VOICE_DIR, "银狼-e10.ckpt"))
    tts.load_sovits_model(os.path.join(VOICE_DIR, "银狼_e10_s120.pth"))
    load_s = round(time.perf_counter() - t0, 2)

    report = {"load_models_s": load_s, "samples": []}
    if torch.cuda.is_available():
        torch.cuda.reset_peak_memory_stats()

    for emotion, (file_name, ref_text) in REFS.items():
        ref_path = os.path.join(REF_DIR, file_name)
        tts.cache_spk_audio(ref_path)
        tts.cache_prompt_audio(ref_path, ref_text, prompt_language="zh")
        for index, line in enumerate(LINES):
            started = time.perf_counter()
            first_at = None
            clips = []
            for chunk in tts.infer_stream(
                spk_audio_path=ref_path,
                prompt_audio_path=ref_path,
                prompt_audio_text=ref_text,
                text=line,
                text_language="zh",
                prompt_language="zh",
                stream_chunk=25,
                overlap_len=5,
            ):
                if first_at is None:
                    first_at = time.perf_counter() - started
                clips.append(chunk)
            wall = time.perf_counter() - started
            audio_s = sum(getattr(c, "audio_len_s", 0.0) or 0.0 for c in clips)
            out = os.path.join(OUT_DIR, f"{emotion}-{index + 1}.wav")
            # 把分块拼起来存一个 wav（chunk.save 会覆盖，所以先拼 numpy）
            import numpy as np
            import soundfile as sf
            data = np.concatenate([c.audio_data for c in clips])
            sf.write(out, data, clips[0].samplerate)
            report["samples"].append({
                "emotion": emotion,
                "text": line,
                "file": out,
                "ttft_s": round(first_at, 3) if first_at else None,
                "wall_s": round(wall, 3),
                "audio_s": round(audio_s, 3),
                "rtf": round(wall / audio_s, 3) if audio_s else None,
            })

    if torch.cuda.is_available():
        report["vram_peak_mb"] = round(torch.cuda.max_memory_allocated() / 1048576, 1)
    print("__GSVSAMPLES__" + json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
