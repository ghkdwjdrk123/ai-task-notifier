"""0단계 검증용 최소 MCP 서버(stdio, 표준 라이브러리만).

도구 두 개:
- approve: --permission-prompt-tool 대상. AskUserQuestion이면 '폰 답'을 넣어 허용한다.
- ask_user: 후보 B. 모델이 직접 부르는 질문 도구. '폰 답'을 돌려준다.

'폰 답'은 SPIKE_DIR/answer.json이 생길 때까지 기다렸다가 읽는다(최대 SPIKE_WAIT초).
없으면 선택지 마지막 항목을 고른다. 모든 요청은 SPIKE_DIR/log.jsonl에 남긴다.
"""
import json
import os
import sys
import time

SPIKE_DIR = os.environ.get("SPIKE_DIR", ".")
WAIT = float(os.environ.get("SPIKE_WAIT", "3"))
LOG = os.path.join(SPIKE_DIR, "log.jsonl")
ANSWER = os.path.join(SPIKE_DIR, "answer.json")


def log(kind, data):
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps({"t": time.time(), "kind": kind, "data": data}, ensure_ascii=False) + "\n")


def phone_answer(questions):
    """질문 목록을 받아 {질문 문장: 고른 라벨} 사전을 돌려준다."""
    log("pending", questions)
    deadline = time.time() + WAIT
    while time.time() < deadline:
        if os.path.exists(ANSWER):
            with open(ANSWER, encoding="utf-8") as f:
                ans = json.load(f)
            os.remove(ANSWER)
            log("phone", ans)
            return ans
        time.sleep(0.2)
    ans = {}
    for q in questions:
        opts = q.get("options") or []
        ans[q.get("question", "")] = opts[-1]["label"] if opts else "알아서 해 주세요"
    log("auto", ans)
    return ans


TOOLS = [
    {
        "name": "approve",
        "description": "Permission prompt handler.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "input": {"type": "object"},
                "tool_use_id": {"type": "string"},
            },
            "required": ["tool_name", "input"],
        },
    },
    {
        "name": "ask_user",
        "description": "Ask the user a decision question and wait for their answer from their phone.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["question", "options"],
        },
    },
]


def call_tool(name, args):
    log("call", {"name": name, "args": args})
    if name == "approve":
        tool = args.get("tool_name")
        inp = args.get("input") or {}
        if tool == "AskUserQuestion":
            answers = phone_answer(inp.get("questions") or [])
            result = {"behavior": "allow", "updatedInput": {**inp, "answers": answers}}
        else:
            result = {"behavior": "deny", "message": f"검증 중이라 {tool}은 허용하지 않음"}
        log("approve_result", result)
        return json.dumps(result, ensure_ascii=False)
    if name == "ask_user":
        q = {"question": args.get("question", ""), "options": [{"label": o} for o in args.get("options", [])]}
        ans = phone_answer([q])
        return f"사용자 답: {ans.get(q['question'], '')}"
    raise ValueError(f"unknown tool {name}")


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        mid = msg.get("id")
        method = msg.get("method")
        if mid is None:
            continue  # 알림은 응답하지 않음
        try:
            if method == "initialize":
                result = {
                    "protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "spike", "version": "0.1"},
                }
            elif method == "tools/list":
                result = {"tools": TOOLS}
            elif method == "tools/call":
                text = call_tool(msg["params"]["name"], msg["params"].get("arguments") or {})
                result = {"content": [{"type": "text", "text": text}]}
            elif method == "ping":
                result = {}
            else:
                raise ValueError(f"unsupported {method}")
            out = {"jsonrpc": "2.0", "id": mid, "result": result}
        except Exception as e:  # noqa: BLE001
            out = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32603, "message": str(e)}}
        sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
