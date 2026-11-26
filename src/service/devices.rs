//! 기기 등록(1회용 코드)·인증·해제와 푸시 구독.

use super::{Service, ServiceError, conflict, new_id};
use crate::clock::to_datetime;
use crate::push::PushSubscription;
use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection};
use sqlx::sqlite::SqliteRow;

/// 읽기 쉬운 문자만(0·O·1·I·L·U 제외).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const CODE_LEN: usize = 10;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked: bool,
    pub has_push: bool,
}

fn device_from_row(r: &SqliteRow) -> Result<DeviceView> {
    Ok(DeviceView {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        created_at: to_datetime(r.try_get("created_at")?),
        last_seen_at: r
            .try_get::<Option<i64>, _>("last_seen_at")?
            .map(to_datetime),
        revoked: r.try_get::<Option<i64>, _>("revoked_at")?.is_some(),
        has_push: r.try_get::<i64, _>("has_push")? != 0,
    })
}

macro_rules! device_select {
    () => {
        "SELECT d.id, d.name, d.created_at, d.last_seen_at, d.revoked_at, \
         EXISTS (SELECT 1 FROM push_subscriptions p WHERE p.device_id = d.id) AS has_push FROM devices d"
    };
}

fn sha256_hex(prefix: &str, s: &str) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn random_code() -> Result<String> {
    let mut out = String::with_capacity(CODE_LEN + 1);
    while out.len() < CODE_LEN + 1 {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("난수 생성 실패: {e}"))?;
        for x in b {
            // 30의 배수(240) 미만만 써서 치우침을 없앤다
            if x < 240 && out.len() < CODE_LEN + 1 {
                if out.len() == 5 {
                    out.push('-');
                }
                out.push(CODE_ALPHABET[(x % 30) as usize] as char);
            }
        }
    }
    Ok(out)
}

fn random_token() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("난수 생성 실패: {e}"))?;
    Ok(hex::encode(b))
}

/// 브라우저 푸시 서비스 호스트(정확히 같거나 하위 도메인). 이 밖의 주소로는 맥이 요청을 보내지 않는다
/// (기기 쿠키가 탈취돼도 맥을 내부망 요청 발판으로 쓰지 못하게).
const PUSH_HOSTS: &[&str] = &[
    "fcm.googleapis.com",        // Chrome·Edge(Android 포함)·삼성 인터넷
    "push.services.mozilla.com", // Firefox
    "web.push.apple.com",        // Safari
    "notify.windows.com",        // 구형 Edge
];

pub(crate) fn allowed_push_endpoint(endpoint: &str) -> bool {
    if endpoint.len() > 2048
        || endpoint
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 사용자 정보(@)·포트(:)가 붙은 주소는 받지 않는다
    if host.is_empty() || host.contains(['@', ':']) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    PUSH_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// base64url(패딩 선택) 디코드 후 길이 검사.
fn b64url_len(s: &str) -> Option<usize> {
    let t = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .ok()
        .map(|v| v.len())
}

/// 등록 코드 맞히기 방지: 10분 안에 틀린 코드가 이만큼이면 쓰지 않은 코드를 모두 무효로 한다.
const PAIR_FAIL_LIMIT: i64 = 10;
const PAIR_FAIL_WINDOW_MS: i64 = 10 * 60_000;

/// 틀린 코드 하나를 센다. 한도에 닿아 남은 코드를 무효로 했으면 true.
async fn record_pairing_failure(conn: &mut SqliteConnection, now: i64) -> Result<bool> {
    let since: Option<i64> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key = 'pair_fail_since'")
            .fetch_optional(&mut *conn)
            .await?;
    let count: i64 = sqlx::query_scalar("SELECT value FROM meta WHERE key = 'pair_fail_count'")
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or(0);
    let (since, count) = match since {
        Some(s) if now - s <= PAIR_FAIL_WINDOW_MS => (s, count + 1),
        _ => (now, 1),
    };
    for (k, v) in [("pair_fail_since", since), ("pair_fail_count", count)] {
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(k)
        .bind(v)
        .execute(&mut *conn)
        .await?;
    }
    if count < PAIR_FAIL_LIMIT {
        return Ok(false);
    }
    sqlx::query("UPDATE pairing_codes SET expires_at = ? WHERE used_at IS NULL AND expires_at > ?")
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    // 새로 만든 코드는 다시 쓸 수 있게 창을 비운다
    sqlx::query("DELETE FROM meta WHERE key IN ('pair_fail_since','pair_fail_count')")
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

impl Service {
    /// 1회용 등록 코드를 만든다. 원문은 이 반환값으로만 볼 수 있다.
    pub async fn create_pairing_code(&self, ttl_ms: i64) -> Result<String> {
        let code = random_code()?;
        let now = self.now();
        sqlx::query(
            "INSERT INTO pairing_codes (code_hash, created_at, expires_at) VALUES (?, ?, ?)",
        )
        .bind(sha256_hex("pair:", &normalize_code(&code)))
        .bind(now)
        .bind(now + ttl_ms)
        .execute(&self.pool)
        .await?;
        Ok(code)
    }

    /// 코드를 기기 토큰으로 바꾼다. 성공한 코드는 다시 쓸 수 없다(T01).
    pub async fn exchange_pairing_code(
        &self,
        code: &str,
        name: &str,
    ) -> Result<(DeviceView, String), ServiceError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(ServiceError::Invalid(
                "기기 이름은 1~60자여야 합니다".into(),
            ));
        }
        let hash = sha256_hex("pair:", &normalize_code(code));
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM pairing_codes WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?",
        )
        .bind(&hash)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        if found.is_none() {
            let burned = record_pairing_failure(&mut tx, now).await?;
            tx.commit().await?;
            return Err(if burned {
                conflict(
                    "too_many_attempts",
                    "틀린 코드가 너무 많아 남은 등록 코드를 모두 무효로 했습니다. 맥에서 새 코드를 만드세요",
                )
            } else {
                conflict(
                    "invalid_code",
                    "등록 코드가 틀렸거나 만료됐거나 이미 쓰였습니다",
                )
            });
        }
        let id = new_id();
        let token = random_token()?;
        sqlx::query("INSERT INTO devices (id, name, token_hash, created_at, last_seen_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(sha256_hex("device:", &token))
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE pairing_codes SET used_at = ?, device_id = ? WHERE code_hash = ?")
            .bind(now)
            .bind(&id)
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(concat!(device_select!(), " WHERE d.id = ?"))
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        let view = device_from_row(&row)?;
        tx.commit().await?;
        Ok((view, token))
    }

    /// 기기 토큰으로 인증한다. 해제된 기기는 None.
    pub async fn authenticate(&self, token: &str) -> Result<Option<DeviceView>> {
        if token.len() != 64 {
            return Ok(None);
        }
        let now = self.now();
        let hash = sha256_hex("device:", token);
        let row = sqlx::query(concat!(
            device_select!(),
            " WHERE d.token_hash = ? AND d.revoked_at IS NULL"
        ))
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let view = device_from_row(&row)?;
        // 마지막 접속 시각은 1분 단위로만 갱신(매 요청 쓰기 방지)
        if view
            .last_seen_at
            .is_none_or(|t| now - t.timestamp_millis() > 60_000)
        {
            sqlx::query("UPDATE devices SET last_seen_at = ? WHERE id = ?")
                .bind(now)
                .bind(&view.id)
                .execute(&self.pool)
                .await?;
        }
        Ok(Some(view))
    }

    pub async fn list_devices(&self) -> Result<Vec<DeviceView>> {
        let rows = sqlx::query(concat!(device_select!(), " ORDER BY d.created_at"))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(device_from_row).collect()
    }

    /// 기기 연결 해제: 이후 조회·답변을 거절하고, 구독과 보내지 않은 푸시를 지운다(T02).
    pub async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let n =
            sqlx::query("UPDATE devices SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(now)
                .bind(device_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query("DELETE FROM push_subscriptions WHERE device_id = ?")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE push_outbox SET state = 'cancelled' WHERE device_id = ? AND state = 'pending'",
        )
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// 현재 기기의 푸시 구독을 교체한다. 같은 endpoint가 다른 기기에 있으면 옮긴다(T29).
    pub async fn set_push_subscription(
        &self,
        device_id: &str,
        sub: &PushSubscription,
    ) -> Result<(), ServiceError> {
        if !allowed_push_endpoint(&sub.endpoint) {
            return Err(ServiceError::Invalid(
                "푸시 endpoint는 알려진 브라우저 푸시 서비스의 https 주소여야 합니다".into(),
            ));
        }
        if b64url_len(&sub.keys.p256dh) != Some(65) || b64url_len(&sub.keys.auth) != Some(16) {
            return Err(ServiceError::Invalid(
                "푸시 키 형식이 올바르지 않습니다".into(),
            ));
        }
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ? AND device_id <> ?")
            .bind(&sub.endpoint)
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO push_subscriptions (device_id, endpoint, p256dh, auth, updated_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (device_id) DO UPDATE SET endpoint = excluded.endpoint, p256dh = excluded.p256dh, \
             auth = excluded.auth, updated_at = excluded.updated_at",
        )
        .bind(device_id)
        .bind(&sub.endpoint)
        .bind(&sub.keys.p256dh)
        .bind(&sub.keys.auth)
        .bind(now)
        .execute(&mut *tx)
        .await
        .context("푸시 구독 저장 실패")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_push_subscription(&self, device_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM push_subscriptions WHERE device_id = ?")
            .bind(device_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_format() {
        let c = random_code().unwrap();
        assert_eq!(c.len(), 11);
        assert_eq!(&c[5..6], "-");
        assert!(
            c.replace('-', "")
                .bytes()
                .all(|b| CODE_ALPHABET.contains(&b))
        );
        assert_eq!(normalize_code(" abcde-fghjk "), "ABCDEFGHJK");
    }

    #[test]
    fn push_endpoint_allowlist() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/x",
            "https://web.push.apple.com/QG",
        ] {
            assert!(allowed_push_endpoint(ok), "{ok}");
        }
        for bad in [
            "http://fcm.googleapis.com/fcm/send/abc",
            "https://127.0.0.1/x",
            "https://fcm.googleapis.com:8443/x",
            "https://evil@fcm.googleapis.com/x",
            "https://fcm.googleapis.com.evil.example/x",
            "https://evilfcm.googleapis.com.example/x",
            "https://mac.tailnet.ts.net/x",
        ] {
            assert!(!allowed_push_endpoint(bad), "{bad}");
        }
    }
}
