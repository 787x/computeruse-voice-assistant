import json
raw = open("ttft.json","rb").read()
for enc in ("utf-16","utf-8-sig","utf-8"):
    try:
        d = json.loads(raw.decode(enc)); break
    except Exception: d = None
if d is None:
    print("cannot decode"); raise SystemExit(1)
print(f'{"case":<30}{"KB":>6}{"conn":>7}{"ttfb":>7}{"TTFT":>8}{"total":>7}')
for r in d["results"]:
    f = lambda k: str(r[k]["median"]) if isinstance(r[k], dict) else str(r[k])
    print(f'{r["case"]:<30}{f("payloadKB"):>6}{f("t_conn"):>7}{f("t_ttfb"):>7}{f("t_ttft"):>8}{f("t_total"):>7}')
