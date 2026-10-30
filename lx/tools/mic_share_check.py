"""验证多个进程能否同时采集麦克风（WASAPI 共享模式）。

背景：用户关心"常驻唤醒词监听会不会独占麦克风、影响其他应用"。
做法：同时启动两个子进程各自开流录音，若双方都拿到非静音数据 → 共享模式成立。
（PowerShell 的 Start-Process 在本机因 env 键大小写重复报错，故用 Python 直接调度。）
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
TARGET = os.path.join(HERE, "mic_concurrent_test.py")

# 清理大小写重复的环境变量键（NO_PROXY / no_proxy 冲突）
clean_env = {}
seen = set()
for k, v in os.environ.items():
    if k.lower() in seen:
        continue
    seen.add(k.lower())
    clean_env[k] = v
clean_env["PYTHONIOENCODING"] = "utf-8"

procs = []
for label in ("A", "B"):
    p = subprocess.Popen([sys.executable, TARGET, label],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         text=True, encoding="utf-8", errors="replace", env=clean_env)
    procs.append((label, p))

results = []
for label, p in procs:
    out, err = p.communicate(timeout=60)
    line = ""
    for ln in (out or "").splitlines():
        if ln.strip().startswith("{"):
            line = ln.strip()
    if line:
        results.append(json.loads(line))
    else:
        results.append({"label": label, "error": (err or "").strip()[-300:] or "no output"})

ok = all(r.get("non_silent") for r in results) and len(results) == 2
print(json.dumps({"concurrent_ok": ok, "results": results}, ensure_ascii=False, indent=1))
