"""후보 C 검증: PreToolUse 훅이 AskUserQuestion에 '폰 답'을 넣는다.

stdin 전체를 SPIKE_DIR/hook_log.jsonl에 남기고, SPIKE_HOOK_WAIT초 기다린 뒤
첫 번째 선택지로 답한다(A 검증의 '마지막 선택지'와 구별하려고).
"""
import json
import os
import sys
import time

spike_dir = os.environ["SPIKE_DIR"]
data = json.load(sys.stdin)
with open(os.path.join(spike_dir, "hook_log.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps({"t": time.time(), "stdin": data}, ensure_ascii=False) + "\n")

if data.get("tool_name") != "AskUserQuestion":
    sys.exit(0)

time.sleep(float(os.environ.get("SPIKE_HOOK_WAIT", "0")))
inp = data.get("tool_input") or {}
answers = {q["question"]: q["options"][0]["label"] for q in inp.get("questions", []) if q.get("options")}
out = {
    "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "updatedInput": {**inp, "answers": answers},
    }
}
with open(os.path.join(spike_dir, "hook_log.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps({"t": time.time(), "out": out}, ensure_ascii=False) + "\n")
print(json.dumps(out, ensure_ascii=False))
