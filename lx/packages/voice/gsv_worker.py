"""
GSV-TTS-Lite 常驻合成 worker。

为什么是常驻进程 + stdin/stdout JSON 行：
  - 模型加载 6.5 秒、显存 1.5GB，只能加载一次，不能每句重建
  - 不走 HTTP：少一层序列化，也少一个端口和一套错误码
  - 独立进程：Python 崩了不会带崩 Electron 主进程，主进程也能随时杀掉它回收显存

协议（每行一个 JSON）：
  入  {"id":"1","op":"speak","text":"...","emotion":"中立","speed":1.0}
      {"id":"2","op":"ping"} / {"id":"3","op":"shutdown"}
  出  {"type":"ready","voices":["中立","生气",...],"rate":32000}
      {"type":"result","id":"1","ok":true,"path":"...","ms":123,"audioSec":3.1,"rate":32000}
      {"type":"result","id":"2","ok":true,"pong":true}
      {"type":"error","id":"1","message":"..."}
"""
import json
import os
import sys
import time
import traceback
import wave

os.environ.setdefault("PYTHONIOENCODING", "utf-8")


def say(payload):
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def ref_text_of(file_name):
    """【中立】xxx.wav → 参考音频的转录就是】后面的部分"""
    return file_name[file_name.index("】") + 1:file_name.rindex(".wav")]


def trim_silence(samples, rate, threshold=0.015, pad_ms=12):
    """掐掉首尾的静音。

    为什么必须做：GPT-SoVITS 每句前后都会带 100~300ms 的静音，
    逐句播放时这些静音会叠加成"说完一句先愣一下"的听感。
    threshold 用相对峰值的 1%（约 -40dB），pad 保留 25ms 免得把字头/字尾的
    气口剪掉——剪掉起音会听起来"咬字被砍了"。
    """
    import numpy as np
    data = np.asarray(samples, dtype="float32")
    if data.size == 0:
        return data
    peak = float(np.max(np.abs(data)))
    if peak <= 0:
        return data
    gate = max(threshold * peak, 1e-4)
    loud = np.abs(data) > gate
    if not bool(np.any(loud)):
        return data
    first = int(np.argmax(loud))
    last = int(len(loud) - np.argmax(loud[::-1]))
    pad = int(rate * pad_ms / 1000)
    start = max(0, first - pad)
    end = min(len(data), last + pad)
    return data[start:end]


def save_wav(path, samples, rate):
    """float32 → 16bit PCM wav（渲染层用 <audio> 播，16bit 足够，体积也小一半）"""
    import numpy as np
    data = np.asarray(samples, dtype="float32")
    peak = float(np.max(np.abs(data))) if data.size else 0.0
    if peak > 1.0:
        data = data / peak
    pcm = np.clip(data * 32767.0, -32768, 32767).astype("<i2")
    with wave.open(path, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(int(rate))
        handle.writeframes(pcm.tobytes())


def main():
    voice_dir = sys.argv[1]
    out_dir = sys.argv[2]
    os.makedirs(out_dir, exist_ok=True)

    from gsv_tts import TTS

    started = time.perf_counter()
    tts = TTS(use_bert=True)
    tts.load_gpt_model(os.path.join(voice_dir, "银狼-e10.ckpt"))
    tts.load_sovits_model(os.path.join(voice_dir, "银狼_e10_s120.pth"))

    ref_dir = os.path.join(voice_dir, "reference_audios", "中文", "emotions")
    voices = {}
    for file_name in os.listdir(ref_dir):
        if not file_name.lower().endswith(".wav") or "】" not in file_name:
            continue
        emotion = file_name[1:file_name.index("】")]
        voices[emotion] = (os.path.join(ref_dir, file_name), ref_text_of(file_name))

    default_emotion = "中立" if "中立" in voices else next(iter(voices))

    # 只预热默认情绪：六条参考音频全预热要 30 秒以上（实测），
    # 其它情绪等真正用到再缓存，换取启动快 4 倍。
    cached = set()

    def ensure_cached(emotion):
        if emotion in cached:
            return voices[emotion]
        path, text = voices[emotion]
        tts.cache_spk_audio(path)
        tts.cache_prompt_audio(path, text, prompt_language="zh")
        cached.add(emotion)
        return voices[emotion]

    ensure_cached(default_emotion)

    # 预热一次推理：CUDA Graph 捕获和 kernel 选型都发生在第一次调用，
    # 不做的话用户听到的第一句会慢 2~3 倍。
    try:
        tts.infer(spk_audio_path=voices[default_emotion][0], prompt_audio_path=voices[default_emotion][0],
                  prompt_audio_text=voices[default_emotion][1], text="嗯",
                  text_language="zh", prompt_language="zh")
    except Exception:
        pass

    say({
        "type": "ready",
        "voices": sorted(voices.keys()),
        "default": default_emotion,
        "loadMs": round((time.perf_counter() - started) * 1000),
        "rate": 32000,
    })

    counter = 0
    for line in sys.stdin:
        line = line.strip()
        if line == "":
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as error:
            say({"type": "error", "id": None, "message": f"bad json: {error}"})
            continue

        request_id = request.get("id")
        op = request.get("op", "speak")

        if op == "shutdown":
            say({"type": "result", "id": request_id, "ok": True, "bye": True})
            break

        if op == "ping":
            say({"type": "result", "id": request_id, "ok": True, "pong": True})
            continue

        if op != "speak":
            say({"type": "error", "id": request_id, "message": f"unknown op: {op}"})
            continue

        text = str(request.get("text") or "").strip()
        if text == "":
            say({"type": "error", "id": request_id, "message": "empty text"})
            continue

        emotion = request.get("emotion") or default_emotion
        if emotion not in voices:
            emotion = default_emotion
        ref_path, ref_text = ensure_cached(emotion)

        try:
            began = time.perf_counter()
            clip = tts.infer(
                spk_audio_path=ref_path,
                prompt_audio_path=ref_path,
                prompt_audio_text=ref_text,
                text=text,
                text_language="zh",
                prompt_language="zh",
                speed=float(request.get("speed") or 1.0),
            )
            elapsed = time.perf_counter() - began
            counter += 1
            out_path = os.path.join(out_dir, f"gsv-{os.getpid()}-{counter}.wav")
            trimmed = trim_silence(clip.audio_data, clip.samplerate)
            save_wav(out_path, trimmed, clip.samplerate)
            say({
                "type": "result",
                "id": request_id,
                "ok": True,
                "path": out_path,
                "ms": round(elapsed * 1000),
                "audioSec": round(float(len(trimmed)) / float(clip.samplerate), 3),
                "rawSec": round(float(clip.audio_len_s), 3),
                "rate": int(clip.samplerate),
                "emotion": emotion,
            })
        except Exception as error:  # 单句失败不该让 worker 退出
            say({"type": "error", "id": request_id, "message": f"{type(error).__name__}: {error}",
                 "trace": traceback.format_exc()[-800:]})


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        say({"type": "fatal", "message": f"{type(error).__name__}: {error}", "trace": traceback.format_exc()[-1200:]})
        sys.exit(1)
