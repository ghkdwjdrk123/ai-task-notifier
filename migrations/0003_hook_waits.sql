-- Claude Code PermissionRequest 훅이 폰 답을 기다리는 우편함.
-- 훅 프로세스가 행을 만들고 기다리며, 서버(제공자 어댑터)가 answers를 쓰면 훅이 가져가(taken_at) 출력한다.
-- 맥에서 먼저 답했거나 훅이 기다림을 끝내면 gave_up_at을 적는다(그 뒤로는 답을 넣지도 가져가지도 않는다).
CREATE TABLE hook_waits (
    -- 결정의 provider_request_id("perm:<uuid>")
    request_id          TEXT PRIMARY KEY,
    -- Claude Code 원본 세션 ID
    provider_session_id TEXT NOT NULL,
    -- 하위 에이전트 안의 요청이면 그 agent_id, 주 대화면 빈 문자열
    agent_id            TEXT NOT NULL DEFAULT '',
    tool_name           TEXT NOT NULL,
    -- tool_input(answers 제외)의 정규 JSON sha256: 맥에서 답한 뒤 오는 PostToolUse와 짝을 맞춘다
    input_hash          TEXT NOT NULL,
    -- 폰 답(JSON). 서버가 쓴다
    answers             TEXT,
    answered_at         INTEGER,
    taken_at            INTEGER,
    gave_up_at          INTEGER,
    -- 훅이 살아 있다는 표시(5초마다). 끊기면 서버가 '훅 사라짐'으로 정리한다
    last_polled_at      INTEGER,
    created_at          INTEGER NOT NULL
);
CREATE INDEX hook_waits_session ON hook_waits (provider_session_id) WHERE taken_at IS NULL AND gave_up_at IS NULL;
