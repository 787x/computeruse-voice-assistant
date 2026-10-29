import wave
import numpy as np

SR = 16000
SEC = 6.0
rng = np.random.default_rng(7)
# 白噪声 + 轻微低通，模拟风扇/空调类稳态噪声，-42dBFS
x = rng.standard_normal(int(SEC * SR)).astype(np.float32)
k = np.ones(8, dtype=np.float32) / 8
x = np.convolve(x, k, mode="same").astype(np.float32)
x = x / (np.sqrt(np.mean(x ** 2)) + 1e-12) * (10 ** (-42 / 20))
with wave.open("assets/test/noise-synthetic.wav", "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(SR)
    w.writeframes((np.clip(x, -1, 1) * 32767).astype(np.int16).tobytes())
print("synthetic noise written, rms_db=", round(float(20*np.log10(np.sqrt(np.mean(x**2)))), 1))
