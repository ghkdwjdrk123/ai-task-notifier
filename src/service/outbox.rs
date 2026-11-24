//! 푸시 발송 대기열 처리.
//!
//! 보내기 직전에 다시 확인한다: 기기가 해제됐거나, 구독이 없거나, 결정이 이미 끝났거나,
//! 유효 시간이 지났으면 보내지 않고 취소한다. 푸시 본문에는 작업 내용을 넣지 않는다(T30).

use super::Service;
use crate::push::{PushKeys, PushOutcome, PushSender, PushSubscription};
use anyhow::Result;
use sqlx::Row;

const MAX_ATTEMPTS: i64 = 8;
const BASE_BACKOFF_MS: i64 = 5_000;
const MAX_BACKOFF_MS: i64 = 10 * 60_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutboxStats {
    pub sent: usize,
    pub cancelled: usize,
    pub retried: usize,
    pub failed: usize,
}

struct Due {
    id: i64,
    device_id: String,
    decision_id: Option<String>,
    snapshot_version: i64,
    expires_at: i64,
    attempts: i64,
}

impl Service {
    /// 보낼 때가 된 푸시를 최대 `limit`건 처리한다.
    pub async fn process_push_outbox(
        &self,
        sender: &dyn PushSender,
        limit: i64,
    ) -> Result<OutboxStats> {
        let now = self.now();
        let rows = sqlx::query(
            "SELECT id, device_id, decision_id, snapshot_version, expires_at, attempts FROM push_outbox \
             WHERE state = 'pending' AND next_attempt_at <= ? ORDER BY id LIMIT ?",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let due = rows
            .iter()
            .map(|r| {
                Ok(Due {
                    id: r.try_get("id")?,
                    device_id: r.try_get("device_id")?,
                    decision_id: r.try_get("decision_id")?,
                    snapshot_version: r.try_get("snapshot_version")?,
                    expires_at: r.try_get("expires_at")?,
                    attempts: r.try_get("attempts")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut stats = OutboxStats::default();
        for item in due {
            let now = self.now();
            // 보내기 직전 확인(한 번의 조회로)
            let check = sqlx::query(
                "SELECT p.endpoint, p.p256dh, p.auth, \
                   (SELECT state FROM decisions WHERE id = ?) AS decision_state \
                 FROM devices d JOIN push_subscriptions p ON p.device_id = d.id \
                 WHERE d.id = ? AND d.revoked_at IS NULL",
            )
            .bind(&item.decision_id)
            .bind(&item.device_id)
            .fetch_optional(&self.pool)
            .await?;
            let sub = match check.as_ref() {
                None => None, // 해제된 기기이거나 구독 없음
                Some(r) => {
                    let decision_state: Option<String> = r.try_get("decision_state")?;
                    let still_pending = match (&item.decision_id, decision_state.as_deref()) {
                        (Some(_), Some(s)) => matches!(s, "open" | "sending" | "confirming"),
                        (Some(_), None) => false,
                        (None, _) => true,
                    };
                    if still_pending {
                        Some(PushSubscription {
                            endpoint: r.try_get("endpoint")?,
                            keys: PushKeys {
                                p256dh: r.try_get("p256dh")?,
                                auth: r.try_get("auth")?,
                            },
                        })
                    } else {
                        None
                    }
                }
            };
            let Some(sub) = sub else {
                self.mark(item.id, "cancelled", None, item.attempts, now)
                    .await?;
                stats.cancelled += 1;
                continue;
            };
            if item.expires_at <= now {
                self.mark(item.id, "cancelled", Some("expired"), item.attempts, now)
                    .await?;
                stats.cancelled += 1;
                continue;
            }
            let payload = serde_json::json!({
                "type": "decision",
                "decision_id": item.decision_id,
                "v": item.snapshot_version,
            });
            let ttl = ((item.expires_at - now) / 1000).clamp(1, 86_400) as u32;
            let outcome = sender.send(&sub, payload.to_string().as_bytes(), ttl).await;
            let now = self.now();
            match outcome {
                PushOutcome::Sent => {
                    self.mark(item.id, "sent", None, item.attempts + 1, now)
                        .await?;
                    stats.sent += 1;
                }
                PushOutcome::Gone => {
                    sqlx::query(
                        "DELETE FROM push_subscriptions WHERE device_id = ? AND endpoint = ?",
                    )
                    .bind(&item.device_id)
                    .bind(&sub.endpoint)
                    .execute(&self.pool)
                    .await?;
                    self.mark(
                        item.id,
                        "failed",
                        Some("subscription_gone"),
                        item.attempts + 1,
                        now,
                    )
                    .await?;
                    stats.failed += 1;
                }
                PushOutcome::Retry(err) => {
                    let attempts = item.attempts + 1;
                    if attempts >= MAX_ATTEMPTS {
                        self.mark(item.id, "failed", Some(&err), attempts, now)
                            .await?;
                        stats.failed += 1;
                    } else {
                        let backoff = (BASE_BACKOFF_MS << attempts.min(10)).min(MAX_BACKOFF_MS);
                        sqlx::query(
                            "UPDATE push_outbox SET attempts = ?, next_attempt_at = ?, last_error = ? \
                             WHERE id = ? AND state = 'pending'",
                        )
                        .bind(attempts)
                        .bind(now + backoff)
                        .bind(&err)
                        .bind(item.id)
                        .execute(&self.pool)
                        .await?;
                        stats.retried += 1;
                    }
                }
            }
        }
        Ok(stats)
    }

    async fn mark(
        &self,
        id: i64,
        state: &str,
        err: Option<&str>,
        attempts: i64,
        _now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE push_outbox SET state = ?, last_error = COALESCE(?, last_error), attempts = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(state)
        .bind(err)
        .bind(attempts)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
