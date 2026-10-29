# 录 6 秒环境噪声 + 生成不同 SNR 的混合测试素材
import wave, json
import numpy as np
import sounddevice as sd

SR = 16000
SEC = 6.0
print("recording 6s ambient noise ...", flush=True)
rec = sd.rec(int(SEC * SR), samplerate=SR, channels=1, dtype="float32", blocking=True)
noise = rec[:, 0].copy()

def write_wav(path, x, sr=SR):
    x = np.clip(x, -1, 1)
    with wave.open(path, "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
        w.writeframes((x * 32767).astype(np.int16).tobytes())

write_wav("assets/test/noise-dorm.wav", noise)

# 读 TTS 干净语音，重采样到 16k
def read_wav(path):
    with wave.open(path, "rb") as w:
        sr = w.getframerate()
        x = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768
    return x, sr

speech, ssr = read_wav("assets/voice-node.wav")
if ssr != SR:
    n = int(len(speech) * SR / ssr)
    idx = np.linspace(0, len(speech) - 1, n)
    speech = np.interp(idx, np.arange(len(speech)), speech).astype(np.float32)

# 噪声能量
def rms_db(x):
    return 20 * np.log10(np.sqrt(np.mean(x ** 2)) + 1e-12)

meta = {"noise_rms_db": round(float(rms_db(noise)), 1), "speech_rms_db": round(float(rms_db(speech)), 1), "mixes": []}

# 把噪声循环到语音等长，按 SNR 混合
def mix_at_snr(speech, noise, snr_db):
    if len(noise) < len(speech):
        reps = int(np.ceil(len(speech) / len(noise)))
        noise = np.tile(noise, reps)
    noise = noise[:len(speech)]
    ps = np.sqrt(np.mean(speech ** 2))
    pn = np.sqrt(np.mean(noise ** 2)) + 1e-12
    target_pn = ps / (10 ** (snr_db / 20))
    mixed = speech + noise * (target_pn / pn)
    peak = np.abs(mixed).max()
    if peak > 0.99:
        mixed = mixed / peak * 0.99
    return mixed.astype(np.float32)

for snr in (20, 10, 5, 0):
    m = mix_at_snr(speech, noise, snr)
    p = f"assets/test/mix-snr{snr}.wav"
    write_wav(p, m)
    meta["mixes"].append({"snr": snr, "path": p, "sec": round(len(m) / SR, 1), "rms_db": round(float(rms_db(m)), 1)})

write_wav("assets/test/speech-clean.wav", speech)
print(json.dumps(meta, ensure_ascii=False))
