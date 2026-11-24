# ai-task-notifier

맥에서 여러 개 돌려 둔 AI 코딩 에이전트(Claude Code, 이후 Codex) 세션의 **진행 상황**과 **내 답이 필요한 결정 요청**을 휴대폰(Android)으로 받아 보고, 폰에서 답하면 그 답이 원래 세션에 반영되어 작업이 이어지게 하는 개인용 도구입니다.

```
맥: 에이전트 세션 ──질문──▶ 알림 서비스 ──Web Push──▶ 폰(PWA): 잠금화면 알림
                 ◀──답────            ◀──HTTPS(사설망)── 질문 상세에서 선택·전송
```

## 상태

**1단계(상태·API 서버)까지 끝났습니다.** 아직 실제 에이전트와 폰에 연결되지 않아 그대로는 쓸 수 없습니다(2단계에서 연결).

| 단계 | 내용 | 상태 |
|---|---|---|
| 0 | 에이전트 질문 왕복, 폰 Web Push 실기 검증 | 완료(일부 측정은 4단계로) |
| 1 | 상태·API 서버(가짜 제공자로 검증) | 완료: 인수 테스트 T01~T10·T12·T13·T16~T20 자동화 |
| 2 | Claude Code 한 세션 왕복 + PWA 화면 | 예정 |
| 3 | Codex 연동, 동시 작업 | 예정 |
| 4 | 외부 접속·알림 안정성 측정 | 예정 |

자세한 요구사항, 상태 모델, 인수 테스트, 검증 결과는 [docs/design.md](docs/design.md)에 있습니다.

## 설계 요점

- **폰 앱은 PWA + Web Push(VAPID)**: 네이티브 앱·Firebase 계정 없이 Chrome에서 잠금화면 알림까지 동작함을 실기로 확인했습니다.
- **에이전트 질문 가로채기**: 공식 `claude` CLI를 헤드리스로 띄우고 `--permission-prompt-tool`로 `AskUserQuestion`을 받아 폰의 답을 `updatedInput.answers`로 돌려줍니다. 평소 대화형 세션은 `PreToolUse` 훅으로 같은 일을 할 수 있습니다.
- **접속**: 맥의 서비스는 `127.0.0.1`에서만 듣고, 폰은 Tailscale 같은 사설망의 HTTPS로만 들어옵니다. 푸시 페이로드와 잠금화면 문구에는 작업 내용을 넣지 않습니다.
- **하지 않는 것**: 구독 로그인 토큰을 꺼내 API를 직접 부르기, 터미널에 문자열을 흘려 넣는 자동화.

## 저장소 구성

| 경로 | 내용 |
|---|---|
| `src/service/` | 상태 계산·전이: 제공자 이벤트(`events.rs`), 답 접수·전달·반영 확인(`answers.rs`), 기기·등록(`devices.rs`), 푸시 대기열(`outbox.rs`), 스냅샷·상세(`snapshot.rs`) |
| `src/api.rs` | 폰(PWA)용 HTTP API |
| `src/provider.rs`, `src/push.rs` | 제공자 연결·푸시 발송 계약과 시험용 가짜 구현 |
| `migrations/` | SQLite 스키마 |
| `tests/` | 인수 테스트(`acceptance.rs`, 시험 이름의 tNN = 설계 문서 테스트 ID)와 API 시험 |
| `docs/design.md` | 설계 문서(요구사항 F01~F14, 상태 모델, 인수 테스트, 0단계 결과) |
| `spikes/claude-question/` | 0단계 검증 스크립트: 질문을 가로채 답을 넣는 최소 MCP 서버(`ask_mcp.py`), PreToolUse 훅(`hook_ask.py`) |
| `spikes/webpush/` | 0단계 검증용 Web Push 서버(`server.mjs`, Node 내장 crypto로 RFC 8291·8292 직접 구현)와 자체 시험(`selftest.mjs`) |

## 서비스 빌드·실행

요구사항: Rust 1.88 이상(edition 2024).

```bash
cargo test
```

```bash
cargo build --release
```

폰 등록 코드 발급(10분 유효, 한 번만 사용):

```bash
./target/release/notifier pair
```

서버 실행. 루프백에서만 듣고, 폰은 Tailscale Serve 같은 사설망 HTTPS로 들어옵니다. `--allowed-origin`에는 폰이 접속할 주소를 줍니다.

```bash
./target/release/notifier serve --allowed-origin https://<맥 이름>.<tailnet>.ts.net
```

기기 목록과 해제:

```bash
./target/release/notifier devices
```

```bash
./target/release/notifier revoke <기기 ID>
```

DB는 기본으로 `~/.local/share/ai-task-notifier/notifier.db`(권한 600)에 생기며 `--db` 또는 `NOTIFIER_DB`로 바꿀 수 있습니다.

### API(1단계)

| 경로 | 내용 |
|---|---|
| `POST /v1/pairing/exchange` | `{code, name}` → 기기 쿠키(HttpOnly·Secure·SameSite=Lax) |
| `GET /v1/snapshot` | 진행 수·결정 대기 수·세션·열린 결정·갱신 지연·스냅샷 버전 |
| `GET /v1/decisions/{id}` | 질문·선택지·남은 상태·답할 수 있는지·마지막 제출 |
| `POST /v1/decisions/{id}/answers` | `{submission_id, decision_version, answers}` → 202(접수) 또는 200(같은 제출 재전송) |
| `GET /v1/decisions/{id}/answers/{submission_id}` | 제출 상태 |
| `PUT`·`DELETE /v1/device/push-subscription` | 브라우저 `PushSubscription.toJSON()` 그대로 |

쓰기 요청은 `Origin`이 `--allowed-origin` 중 하나여야 합니다. 오류는 `{error, message}` JSON입니다.

## 0단계 시험 코드 실행

시험용 코드입니다. 인증이 없으니 **공개 인터넷에 노출하지 마세요**(Tailscale Funnel 등 금지).

### Web Push

요구사항: Node 20 이상(외부 패키지 없음), 폰에서 접속할 HTTPS 주소(예: Tailscale Serve).

```bash
node spikes/webpush/selftest.mjs
```

```bash
node spikes/webpush/server.mjs ./data 8766
```

```bash
tailscale serve --bg --https=8443 http://127.0.0.1:8766
```

폰 Chrome에서 `https://<맥 이름>.<tailnet>.ts.net:8443`을 열고 "알림 허용하고 구독" → "시험 알림"을 누릅니다. VAPID 키·구독·로그는 `./data`(권한 600)에 생기며 커밋하지 않습니다. 끝나면 `tailscale serve --https=8443 off`로 닫습니다.

> Chromium 계열 브라우저는 알림 권한 요청을 여러 번 거부하면 그 사이트를 자동 차단합니다. 이때는 브라우저 설정 → 사이트 설정 → 해당 사이트 → "삭제 및 재설정"으로 풀어야 합니다.

### Claude Code 질문 왕복

요구사항: Claude Code CLI(로그인 필요), Python 3. 빈 임시 폴더에서 실행합니다. 경로는 자기 환경에 맞게 바꾸세요.

```bash
cat > mcp.json <<EOF
{"mcpServers":{"spike":{"command":"python3","args":["-I","/path/to/spikes/claude-question/ask_mcp.py"],"env":{"SPIKE_DIR":"$PWD","SPIKE_WAIT":"60"}}}}
EOF
```

```bash
claude -p "반드시 AskUserQuestion 도구로 '중복 발견 시 어떻게 할까요?'라고 묻고 선택지는 '전체 중단'과 '중복 행만 건너뛰기'로 해. 답을 받으면 고른 선택지를 한 줄로 말해." --strict-mcp-config --mcp-config mcp.json --permission-prompt-tool mcp__spike__approve --tools AskUserQuestion --output-format stream-json --verbose < /dev/null
```

기다리는 동안 같은 폴더에 `answer.json`(`{"중복 발견 시 어떻게 할까요?": "전체 중단"}`)을 만들면 그 답이 들어가고, 없으면 마지막 선택지로 자동 응답합니다. 요청·응답은 `log.jsonl`에 남습니다.

## 라이선스

[MIT](LICENSE) © 2026 ghkdwjdrk123

이 저장소의 코드는 Python 표준 라이브러리와 Node 내장 모듈만 사용하며, 외부 코드를 포함하지 않습니다. Claude Code, Codex, Chrome, Tailscale은 각 회사의 제품이며 이 프로젝트는 그 회사들과 관계가 없습니다.
