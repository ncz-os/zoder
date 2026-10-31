#!/bin/bash
# Measure a zoder command on a synthetic N-row ledger in an isolated HOME on local disk.
set -u
BIN="$1"; N="${2:-100000}"; REPS="${3:-7}"; CMD="${4:-spend month}"
BASE="$HOME/hive-workspaces/01a190eb-45dc-7385-8154-97e1bc71763d/bench-tmp"
H="$BASE/home-$N"
mkdir -p "$H/.zoder"
cp "$HOME/hive-workspaces/01a190eb-45dc-7385-8154-97e1bc71763d/corpus/model_corpus.json" "$H/.zoder/model_corpus.json"
if [ ! -s "$H/.zoder/ledger.jsonl" ]; then
/usr/bin/python3 - "$H/.zoder/ledger.jsonl" "$N" <<'PY'
import json, sys, datetime
path, n = sys.argv[1], int(sys.argv[2])
start = datetime.datetime(2026,9,1,0,0,0)
with open(path,"w") as f:
    for i in range(n):
        ts = start + datetime.timedelta(seconds=2*i)
        f.write(json.dumps({"ts_utc": ts.strftime("%Y-%m-%dT%H:%M:%SZ"),"provider":"p%d"%(i%7),
                 "model":"vendor/model-%d"%(i%53),"host":"vendor",
                 "tokens_in":100+i%900,"tokens_out":50+i%400,
                 "cost_usd":round(0.0001*(1+i%50),6),"cost_unknown":False,"calls":1},
                 separators=(",",":"))+"\n")
PY
fi
stat -c 'ledger bytes: %s rows: '"$N" "$H/.zoder/ledger.jsonl"
for i in $(seq 1 "$REPS"); do
  S=$(date +%s%N)
  HOME="$H" "$BIN" $CMD >/dev/null 2>&1
  E=$(date +%s%N)
  echo "$(( (E-S)/1000000 )) ms"
done
