# ai-task-notifier

맥에서 여러 개 돌려 둔 AI 코딩 에이전트(Claude Code, 이후 Codex) 세션의 **진행 상황·결정 요청·구독 사용량**을 휴대폰(Android) 알림창에 **상시 표시**하고, 누르면 결정 요청에 답해 원래 세션이 이어지게 하는 개인용 도구입니다. 푸시는 쓰지 않습니다.

```
맥: 에이전트 세션 ──질문·상태·사용량──▶ notifier 서버 ◀──실시간 스트림(SSE, Tailscale HTTPS)── 폰 앱: 상시 알림
                 ◀─────────답──────────            ◀──────────답변───────────────────────── 누르면 답변 화면
```

상시 알림 예: `진행 3 · 결정 대기 1 · 지시 대기 2` / `Claude 5시간 48% · 주간 4%  |  ChatGPT 5시간 12% · 주간 31%`

## 상태

**실제 Claude Code·Codex의 세션 상태(진행·결정 대기·지시 대기)와 구독 사용량을 폰 상시 알림에 보여 주고, Claude Code의 질문·실행 권한에는 폰에서 답할 수 있습니다**(맥 화면과 먼저 답한 쪽이 쓰임). Codex는 관측만 합니다.

| 단계 | 내용 | 상태 |
|---|---|---|
| 0 | 에이전트 질문 왕복, 폰 Web Push 실기 검증 | 완료(일부 측정은 4단계로) |
| 1 | 상태·API 서버(가짜 제공자로 검증) | 완료: 인수 테스트 T01~T10·T12·T13·T16~T20 자동화 |
| 2a | 상시 알림 Android 앱, 사용량·실시간 스트림·앱 인증 | 완료(실기 설치 전) |
| 2b | 실제 데이터 수집: Claude Code 세션(훅)·Claude 사용량(`/usage`)·Codex 세션·사용량(기록 파일) | 완료(관측만) |
| 2c | 폰에서 보낸 답을 실제 Claude Code 세션에 넣기(질문·실행 권한) | 완료(Codex는 관측만) |
| 2d | '지시 대기' 상태(턴을 마치고 다음 지시를 기다리는 세션) | 완료 |
| 3 | Codex 연동, 동시 작업 | 관측만 완료(Codex에 폰으로 답하는 공식 경로 없음) |
| 4 | 외부 접속·알림 안정성 측정, 기록 정리 | 기록 정리·등록 시도 제한 완료, 실기 측정 예정 |

자세한 요구사항, 상태 모델, 인수 테스트, 검증 결과는 [docs/design.md](docs/design.md)에 있습니다.

## 실제 데이터 수집

| 대상 | 방법 | 비고 |
|---|---|---|
| Claude Code 세션 | Claude Code 훅 → `notifier hook claude` → DB | 세션 시작·프롬프트·멈춤·종료·권한 요청·질문(AskUserQuestion)·도구 완료. 제목은 작업 폴더 이름만, 프롬프트 내용은 저장하지 않음 |
| Claude Code 질문·권한에 폰으로 답하기 | `PermissionRequest` 훅이 결정 요청을 만들고 폰 답을 기다림 | 맥 화면의 창도 함께 떠 있고 **먼저 온 답이 쓰임**. 폰 화면에는 질문과 선택지, 실행 권한이면 명령·파일 경로가 보임(하루 뒤 우편함 삭제) |
| Claude 사용량 | 서버가 5분마다 `claude -p /usage` 실행 | 모델 호출 없음. `--claude-usage <CLAUDE_CONFIG_DIR>`의 로그인 사용. 출력 문구가 바뀌면 갱신이 멈출 수 있음 |
| Codex 세션·사용량 | 서버가 5초마다 `~/.codex/sessions/**/rollout-*.jsonl`의 새 줄을 읽음 | 최근 6시간 파일만. 시작 때 가장 최근 파일의 마지막 사용량을 '○시간 전' 값으로 표시 |

Claude Code 전역 설정(`~/.claude/settings.json`)에 넣는 훅. 관측용(각 이벤트에 추가):

```json
{"type": "command", "command": "/Users/<나>/.local/bin/notifier hook claude", "timeout": 5}
```

이벤트: `SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure`, `SessionEnd`, `Notification`, `PreToolUse`(matcher `AskUserQuestion`), `PostToolUse`·`PostToolUseFailure`(matcher `*`).

`PermissionRequest`(matcher `*`)는 폰 답을 기다리므로 **시간 제한을 길게** 따로 준다. `--wait-secs`는 `timeout`보다 30초쯤 짧게(시간 제한에 걸려 훅이 강제로 끝나면 폰 요청은 서버가 15초 안에 정리한다):

```json
{"type": "command", "command": "/Users/<나>/.local/bin/notifier hook claude --wait-secs 1170", "timeout": 1200}
```

이 훅은 맥 화면의 창을 막지 않는다(창과 동시에 돌고 먼저 온 답이 쓰임). 1200초 대기 후 답이 반영되는 것을 실측했다.

서버 실행 예:

```bash
notifier serve --claude-usage ~/.config/<전용 폴더>/claude --claude-bin ~/.local/bin/claude --codex ~/.codex
```

## 설계 요점

- **폰 앱은 Android 상시 알림**: 푸시 대신 소리 없는 상태 알림 하나가 계속 떠 있고(진행 수·결정 대기 수·사용량), 누르면 답변 화면으로 들어갑니다. Web Push 경로(0단계 검증)는 코드만 남아 있습니다.
- **평소 세션에 답 넣기**: Claude Code의 `PermissionRequest` 훅은 맥 화면의 질문·권한 창과 **동시에** 돌고, 훅이 결정 JSON을 내면 그 답이 쓰입니다(대화형 TUI 실측). 훅 프로세스가 DB 우편함에서 폰 답을 기다렸다가 `AskUserQuestion`이면 `updatedInput.answers`, 실행 권한이면 `allow`/`deny`로 돌려줍니다. 맥에서 먼저 답하면 뒤따르는 `PostToolUse`·`Stop` 훅으로 알아채 폰 쪽 요청을 '다른 곳에서 답함'으로 닫습니다.
- **접속**: 맥의 서비스는 `127.0.0.1`에서만 듣고, 폰은 Tailscale 같은 사설망의 HTTPS로만 들어옵니다. 푸시 페이로드와 잠금화면 문구에는 작업 내용을 넣지 않습니다.
- **하지 않는 것**: 구독 로그인 토큰을 꺼내 API를 직접 부르기, 터미널에 문자열을 흘려 넣는 자동화.

## 저장소 구성

| 경로 | 내용 |
|---|---|
| `src/service/` | 상태 계산·전이: 제공자 이벤트(`events.rs`), 답 접수·전달·반영 확인(`answers.rs`), 기기·등록(`devices.rs`), 스냅샷·상세(`snapshot.rs`) |
| `src/api.rs` | 폰 앱용 HTTP API(실시간 스트림 포함) |
| `src/collect.rs` | 실제 데이터 수집: Claude Code 훅, Claude `/usage`, Codex 기록 파일 |
| `src/claude_answer.rs` | 폰 답을 Claude Code 세션에 넣기: `PermissionRequest` 훅 우편함과 서버 어댑터 |
| `src/provider.rs` | 제공자 연결 계약과 시험용 가짜 제공자 |
| `android/` | Android 앱(Kotlin, 외부 라이브러리 없음): 상시 알림 서비스, 등록·대시보드·답변 화면 |
| `migrations/` | SQLite 스키마 |
| `tests/` | 인수 테스트(`acceptance.rs`, 시험 이름의 tNN = 설계 문서 테스트 ID), API 시험, 수집(`collect.rs`)·답 전달(`claude_answer.rs`) 시험 |
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

### Android 앱

요구사항: JDK 17, Android SDK(platform 36, build-tools 36.1.0). `android/local.properties`에 `sdk.dir=<SDK 경로>`.

```bash
cd android && ./gradlew :app:assembleDebug
```

결과: `android/app/build/outputs/apk/debug/app-debug.apk`. 폰에 설치한 뒤 서버 주소(Tailscale Serve HTTPS)와 `notifier pair` 코드를 넣으면 상시 알림이 뜹니다. 화면이 꺼져도 연결을 유지하려면 앱 안내에 따라 배터리 최적화를 해제하세요.

**앱 안에서 업데이트**(0.2.0부터): 새 버전을 빌드해 서버의 앱 업데이트 폴더(기본 DB 옆 `app/`)에 올리면, 앱 대시보드에 '새 버전 … 업데이트 설치' 카드가 뜹니다(10분마다 확인, 설정의 '업데이트 확인'으로 바로). 앱은 등록된 기기 토큰으로 받고 크기·SHA-256을 확인한 뒤 안드로이드 설치 화면으로 넘깁니다. 처음 한 번은 이 앱의 '출처를 알 수 없는 앱 설치' 허용이 필요합니다. 기존 앱과 같은 키로 서명된 APK만 설치됩니다.

```bash
android/publish.sh   # 빌드 → ~/.local/share/ai-task-notifier/app/latest.apk·latest.json
```

실제 어댑터 없이 시험하려면 데모 제공자로 서버를 띄우고 이벤트를 넣습니다.

```bash
./target/release/notifier serve --demo-provider demo
```

```bash
echo '{"provider":"demo","session_id":"m1","event_id":"e1","type":"turn_started"}' | ./target/release/notifier ingest
```

### API

| 경로 | 내용 |
|---|---|
| `POST /v1/pairing/exchange` | `{code, name}` → 기기 쿠키(HttpOnly·Secure·SameSite=Lax) |
| `GET /v1/snapshot` | 진행 수·결정 대기 수·세션·열린 결정·갱신 지연·스냅샷 버전 |
| `GET /v1/decisions/{id}` | 질문·선택지·남은 상태·답할 수 있는지·마지막 제출 |
| `POST /v1/decisions/{id}/answers` | `{submission_id, decision_version, answers}` → 202(접수) 또는 200(같은 제출 재전송) |
| `GET /v1/decisions/{id}/answers/{submission_id}` | 제출 상태 |
| `GET /v1/stream` | 실시간 스냅샷(SSE). 버전이 바뀌면 즉시, 아니어도 30초마다 |
| `POST /v1/sessions/{id}/ack` | 지시 대기 세션 '확인함'(지시 대기 수에서 뺌, 다음 턴을 마치면 다시 지시 대기) |
| `DELETE /v1/device` | 이 기기 등록 해제(토큰 무효) |
| `GET /v1/app/latest` | 앱 업데이트 폴더의 최신 버전 `{version_code, version_name, size, sha256}`(없으면 404 `no_app`) |
| `GET /v1/app/apk` | 최신 APK(`application/vnd.android.package-archive`, `x-apk-sha256`) |

인증: 앱은 등록 때 `token_in_body: true`로 토큰을 받아 `Authorization: Bearer`로 보냅니다. 브라우저는 쿠키를 쓰며, 쿠키를 실은 쓰기 요청은 `Origin`이 `--allowed-origin` 중 하나여야 합니다. 오류는 `{error, message}` JSON입니다.

제공자 이벤트(`notifier ingest` 입력, JSON 한 줄에 하나): `session_started`·`turn_started`·`turn_ended`·`subagent_activity`·`permission_requested`·`permission_resolved`·`decision_requested`·`decision_withdrawn`·`decision_answered_elsewhere`·`session_ended`·`heartbeat`·`usage_reported`. 형식은 `src/model.rs`의 `EventKind`.

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
