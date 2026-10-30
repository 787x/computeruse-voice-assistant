"""
GSV-TTS-Lite 正式测量（非流式路径）+ 生成干净试听样本。

为什么要重测：流式分块之间是有重叠的（overlap_len），
把 audio_data 直接拼起来会重复计入重叠部分，时长虚高、RTF 看着偏乐观。
我们的播报本来就是"一句一合成再播"，所以非流式 infer 的数字才是该拿来决策的。
"""
import json
import os
import time

os.environ.setdefault("PYTHONIOENCODING", "utf-8")

VOICE_DIR = r".runtime/voices/v2ProPlus/银狼"
REF_DIR = os.path.join(VOICE_DIR, "reference_audios", "中文", "emotions")
OUT_DIR = r".runtime/samples"

EMOTIONS = {
    "中立": "【中立】前面是空间站的核心区域，反物质军团的虚卒会很多喔。.wav",
    "生气": "【生气】事先说清楚，这只是我的直觉，对你不一定管用，如果没有效果，我不负责。.wav",
    "开心": "【开心】虽然我们的立场不同…但在这样一场宇宙级的盛会上，就先放下那些成见，共同享受游戏的快乐吧——你说呢？.wav",
}

SENTENCES = [
    "好的。",
    "我已经把浏览器打开了，接下来要搜什么？",
    "上海今天多云转晴，气温大概十七到二十一度，早晚有点凉，出门套件薄外套就够了。",
]


def ref_text_of(file_name):
    """文件名里 【情绪】 之后的部分就是参考音频的转录"""
    return file_name[file_name.index("】") + 1:file_name.rindex(".wav")]


def main():
    from gsv_tts import TTS
    import numpy as np
    import soundfile as sf
    import torch

    os.makedirs(OUT_DIR, exist_ok=True)
    tts = TTS(use_bert=True)
    t0 = time.perf_counter()
    tts.load_gpt_model(os.path.join(VOICE_DIR, "银狼-e10.ckpt"))
    tts.load_sovits_model(os.path.join(VOICE_DIR, "银狼_e10_s120.pth"))
    report = {"load_models_s": round(time.perf_counter() - t0, 2), "perf": [], "samples": []}

    if torch.cuda.is_available():
        torch.cuda.reset_peak_memory_stats()

    # 1) 性能：中立音色，三句不同长度，各跑两次（第一次含预热）
    ref_file = EMOTIONS["中立"]
    ref_path = os.path.join(REF_DIR, ref_file)
    ref_text = ref_text_of(ref_file)
    tts.cache_spk_audio(ref_path)
    tts.cache_prompt_audio(ref_path, ref_text, prompt_language="zh")

    tts.infer(spk_audio_path=ref_path, prompt_audio_path=ref_path, prompt_audio_text=ref_text,
              text="预热", text_language="zh", prompt_language="zh")  # 预热：CUDA Graph / kernel 选型

    for text in SENTENCES:
        rounds = []
        for _ in range(2):
            started = time.perf_counter()
            clip = tts.infer(spk_audio_path=ref_path, prompt_audio_path=ref_path, prompt_audio_text=ref_text,
                             text=text, text_language="zh", prompt_language="zh")
            wall = time.perf_counter() - started
            seconds = float(getattr(clip, "audio_len_s", 0) or 0)
            rounds.append({"wall_s": round(wall, 3), "audio_s": round(seconds, 3),
                           "rtf": round(wall / seconds, 3) if seconds else None})
        report["perf"].append({"chars": len(text), "text": text, "rounds": rounds})

    # 2) 样本：三种情绪 × 两句话，干净单块写入
    line = "好的，我把浏览器打开了，要搜什么？"
    for emotion, file_name in EMOTIONS.items():
        path = os.path.join(REF_DIR, file_name)
        text = ref_text_of(file_name)
        tts.cache_spk_audio(path)
        tts.cache_prompt_audio(path, text, prompt_language="zh")
        clip = tts.infer(spk_audio_path=path, prompt_audio_path=path, prompt_audio_text=text,
                         text=line, text_language="zh", prompt_language="zh")
        out = os.path.join(OUT_DIR, f"银狼-{emotion}.wav")
        sf.write(out, np.asarray(clip.audio_data), clip.samplerate)
        report["samples"].append({"emotion": emotion, "file": out,
                                  "audio_s": round(float(clip.audio_len_s), 2), "rate": clip.samplerate})

    if torch.cuda.is_available():
        report["vram_peak_mb"] = round(torch.cuda.max_memory_allocated() / 1048576, 1)
    print("__GSVFINAL__" + json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
