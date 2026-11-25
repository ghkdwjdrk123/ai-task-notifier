-- 제공자별 구독 사용량(창 단위: 예) Claude five_hour·seven_day, Codex primary·secondary).
CREATE TABLE usage_windows (
    provider     TEXT NOT NULL,
    window       TEXT NOT NULL,
    -- 사용 비율(0~100, 소수 허용)
    used_percent REAL NOT NULL,
    -- 다시 채워지는 시각(UTC 밀리초)
    resets_at    INTEGER,
    -- 제공자가 준 상태 문자열(예: allowed, allowed_warning, rejected)
    status       TEXT,
    observed_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, window)
);
