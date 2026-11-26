-- 세션이 지금 상태가 된 시각('12분째 진행 중' 표시). 상태가 바뀌는 모든 UPDATE는 updated_at도
-- 함께 쓰므로, 상태가 바뀔 때 그 값을 옮겨 적는다.
ALTER TABLE sessions ADD COLUMN state_since INTEGER;
UPDATE sessions SET state_since = updated_at;
CREATE TRIGGER sessions_state_since AFTER UPDATE OF state ON sessions
WHEN NEW.state IS NOT OLD.state
BEGIN
    UPDATE sessions SET state_since = NEW.updated_at WHERE id = NEW.id;
END;

-- 제공자별 사용량 수집 실패(예: Claude 로그인 만료). 성공하면 지운다.
ALTER TABLE providers ADD COLUMN usage_error TEXT;
ALTER TABLE providers ADD COLUMN usage_error_at INTEGER;
