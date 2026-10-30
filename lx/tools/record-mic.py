# 用麦克风录一段音频（默认 4 秒），供 KWS / VAD 离线验证
# 用法: python tools/record-mic.py 4 assets/test/my-voice.wav
import sys
import wave

import numpy as np
import sounddevice as sd

SR = 16000
sec = float(sys.argv[1]) if len(sys.argv) > 1 else 4.0
out = sys.argv[2] if len(sys.argv) > 2 else "assets/test/my-voice.wav"

print(f"录音 {sec} 秒…现在开始说唤醒词", flush=True)
rec = sd.rec(int(sec * SR), samplerate=SR, channels=1, dtype="float32", blocking=True)
x = rec[:, 0].copy()
peak = float(np.abs(x).max())
rms = float(np.sqrt(np.mean(x ** 2)))
print(f"完成: peak={peak:.4f} rms_db={20*np.log10(rms+1e-12):.1f}")

with wave.open(out, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(SR)
    w.writeframes((np.clip(x, -1, 1) * 32767).astype(np.int16).tobytes())
print(f"已保存: {out}")
