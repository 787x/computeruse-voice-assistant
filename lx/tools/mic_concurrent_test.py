"""验证 Windows 是否允许多个进程同时采集麦克风（WASAPI 共享模式）。

用途：证明"常驻唤醒词监听"不会独占麦克风、不影响其他应用。
本脚本被同时启动两份，各自开流录 4 秒并输出 RMS/样本数；
若两者都拿到非静音数据，说明共享模式成立。
"""
import json
import sys
import time

import numpy as np
import sounddevice as sd

LABEL = sys.argv[1] if len(sys.argv) > 1 else "A"
SEC = 4.0
SR = 16000


def main():
    t0 = time.perf_counter()
    try:
        rec = sd.rec(int(SEC * SR), samplerate=SR, channels=1, dtype="float32", blocking=True)
    except Exception as e:
        print(json.dumps({"label": LABEL, "error": f"{type(e).__name__}: {e}"}))
        return
    dt = time.perf_counter() - t0
    x = rec[:, 0]
    rms = float(np.sqrt(np.mean(x ** 2)) + 1e-12)
    print(json.dumps({
        "label": LABEL,
        "sec": round(dt, 2),
        "samples": int(len(x)),
        "rms_dbfs": round(20 * np.log10(rms + 1e-12), 1),
        "peak": round(float(np.abs(x).max()), 4),
        "non_silent": bool(rms > 1e-5),
        "device": sd.query_devices(kind="input")["name"],
    }, ensure_ascii=False))


if __name__ == "__main__":
    main()
