-- 시각은 모두 UTC 밀리초(INTEGER)로 저장한다.

-- 전체 스냅샷 버전: 화면에 보이는 상태가 바뀔 때마다 1씩 오른다.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT INTO meta (key, value) VALUES ('snapshot_version', 0);

-- 등록된 폰. 토큰 원문은 저장하지 않고 SHA-256만 둔다.
CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at   INTEGER
);

-- 1회용 등록 코드(원문은 맥 화면에만 보이고 여기에는 해시만).
CREATE TABLE pairing_codes (
    code_hash  TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER,
    device_id  TEXT REFERENCES devices(id)
);

-- 기기당 Web Push 구독 하나.
CREATE TABLE push_subscriptions (
    device_id  TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    endpoint   TEXT NOT NULL UNIQUE,
    p256dh     TEXT NOT NULL,
    auth       TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- 제공자(Claude Code, Codex …)별 마지막 이벤트 시각: 갱신 지연 판단용.
CREATE TABLE providers (
    name          TEXT PRIMARY KEY,
    last_event_at INTEGER NOT NULL
);

-- 최상위 세션. (제공자, 원본 세션 ID)가 식별자다.
CREATE TABLE sessions (
    id                  TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    provider_session_id TEXT NOT NULL,
    title               TEXT NOT NULL,
    state               TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (provider, provider_session_id)
);

-- 결정 요청. (세션, 원본 요청 ID)로 중복을 막는다.
CREATE TABLE decisions (
    id                  TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL REFERENCES sessions(id),
    provider_request_id TEXT NOT NULL,
    title               TEXT NOT NULL,
    questions           TEXT NOT NULL,
    allow_free_text     INTEGER NOT NULL,
    rationale           TEXT,
    created_at          INTEGER NOT NULL,
    deadline_at         INTEGER,
    state               TEXT NOT NULL,
    closed_reason       TEXT,
    version             INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (session_id, provider_request_id)
);
CREATE INDEX decisions_state ON decisions(state);

-- 답변 제출. ID는 폰이 만들어 보내며, 같은 ID 재전송은 기존 결과를 돌려준다.
CREATE TABLE submissions (
    id               TEXT PRIMARY KEY,
    decision_id      TEXT NOT NULL REFERENCES decisions(id),
    decision_version INTEGER NOT NULL,
    answers          TEXT NOT NULL,
    device_id        TEXT,
    state            TEXT NOT NULL,
    detail           TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE INDEX submissions_decision ON submissions(decision_id);
CREATE INDEX submissions_state ON submissions(state);

-- 제공자 이벤트 멱등 키.
CREATE TABLE event_keys (
    provider    TEXT NOT NULL,
    idem_key    TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (provider, idem_key)
);

-- 푸시 발송 대기열. 새 결정과 같은 트랜잭션에서 기록한다.
CREATE TABLE push_outbox (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    device_id        TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL,
    decision_id      TEXT REFERENCES decisions(id),
    snapshot_version INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    state            TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  INTEGER NOT NULL,
    last_error       TEXT,
    created_at       INTEGER NOT NULL,
    UNIQUE (device_id, kind, decision_id)
);
CREATE INDEX push_outbox_due ON push_outbox(state, next_attempt_at);
