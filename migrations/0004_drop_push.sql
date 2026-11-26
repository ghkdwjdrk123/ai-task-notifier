-- Web Push를 쓰지 않게 되어(2026-11-25 상시 알림 앱으로 전환) 구독·발송 대기열을 지운다.
DROP TABLE IF EXISTS push_outbox;
DROP TABLE IF EXISTS push_subscriptions;
