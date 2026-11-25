//! 제공자 이벤트 반영.

use super::{
    DecisionRow, PUSH_MAX_TTL_MS, Service, ServiceError, bump_version, decision_select,
    finish_decision, new_id,
};
use crate::model::{
    CloseReason, DecisionState, EventKind, ProviderEvent, Question, SessionState, UsageWindow,
};
use anyhow::Result;
use sqlx::{Row, SqliteConnection};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestResult {
    /// 같은 멱등 키를 이미 받았다.
    pub duplicate: bool,
    /// 화면에 보이는 상태가 바뀌었다(스냅샷 버전이 올랐다).
    pub changed: bool,
    /// 새로 만든 결정 ID(DecisionRequested일 때).
    pub new_decision: Option<String>,
}

const MAX_QUESTIONS: usize = 10;
const MAX_OPTIONS: usize = 20;
const MAX_TEXT: usize = 4000;
const MAX_TITLE: usize = 200;
const MAX_HEADER: usize = 100;
const MAX_DESCRIPTION: usize = 1000;

fn validate_questions(qs: &[Question], allow_free_text: bool) -> Result<(), String> {
    if qs.is_empty() || qs.len() > MAX_QUESTIONS {
        return Err(format!("질문은 1~{MAX_QUESTIONS}개여야 합니다"));
    }
    let mut seen = HashSet::new();
    for q in qs {
        if q.question.trim().is_empty() || q.question.len() > MAX_TEXT {
            return Err("질문 문장이 비었거나 너무 깁니다".into());
        }
        if q.header.as_ref().is_some_and(|h| h.len() > MAX_HEADER) {
            return Err("질문 머리말이 너무 깁니다".into());
        }
        if !seen.insert(q.question.as_str()) {
            return Err("같은 질문 문장이 두 번 있습니다(답을 질문 문장으로 구분함)".into());
        }
        if q.options.len() > MAX_OPTIONS {
            return Err(format!("선택지는 {MAX_OPTIONS}개 이하여야 합니다"));
        }
        if q.options.is_empty() && !allow_free_text {
            return Err("선택지가 없는 질문은 자유 입력을 허용해야 합니다".into());
        }
        let mut labels = HashSet::new();
        for o in &q.options {
            if o.label.trim().is_empty()
                || o.label.len() > MAX_TEXT
                || !labels.insert(o.label.as_str())
            {
                return Err("선택지 라벨이 비었거나 겹칩니다".into());
            }
            if o.description
                .as_ref()
                .is_some_and(|d| d.len() > MAX_DESCRIPTION)
            {
                return Err("선택지 설명이 너무 깁니다".into());
            }
        }
    }
    Ok(())
}

/// 사용량을 저장한다. 화면에 보이는 값(정수 %·상태·초기화 시각)이 바뀌었으면 true.
async fn record_usage(
    conn: &mut SqliteConnection,
    provider: &str,
    windows: &[UsageWindow],
    now: i64,
) -> Result<bool, ServiceError> {
    if windows.is_empty() || windows.len() > 8 {
        return Err(ServiceError::Invalid("사용량 창은 1~8개여야 합니다".into()));
    }
    let mut changed = false;
    for w in windows {
        let name_ok = !w.name.is_empty()
            && w.name.len() <= 40
            && w.name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok || !w.used_percent.is_finite() || !(0.0..=100.0).contains(&w.used_percent) {
            return Err(ServiceError::Invalid(
                "사용량 창 이름이나 비율(0~100)이 올바르지 않습니다".into(),
            ));
        }
        if w.status.as_ref().is_some_and(|s| s.len() > 40) {
            return Err(ServiceError::Invalid(
                "사용량 상태 문자열이 너무 깁니다".into(),
            ));
        }
        let old = sqlx::query(
            "SELECT used_percent, resets_at, status FROM usage_windows WHERE provider = ? AND window = ?",
        )
        .bind(provider)
        .bind(&w.name)
        .fetch_optional(&mut *conn)
        .await?;
        let differs = match &old {
            None => true,
            Some(r) => {
                let p: f64 = r.try_get("used_percent")?;
                let reset: Option<i64> = r.try_get("resets_at")?;
                let status: Option<String> = r.try_get("status")?;
                p.round() as i64 != w.used_percent.round() as i64
                    || reset != w.resets_at
                    || status != w.status
            }
        };
        changed |= differs;
        sqlx::query(
            "INSERT INTO usage_windows (provider, window, used_percent, resets_at, status, observed_at) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (provider, window) DO UPDATE SET \
             used_percent = excluded.used_percent, resets_at = excluded.resets_at, \
             status = excluded.status, observed_at = excluded.observed_at",
        )
        .bind(provider)
        .bind(&w.name)
        .bind(w.used_percent)
        .bind(w.resets_at)
        .bind(&w.status)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(changed)
}

/// 일하는 세션의 상태: 열린 결정이 있으면 결정 대기(진행 수에서 빠짐), 없으면 진행 중.
async fn active_state(conn: &mut SqliteConnection, session_id: &str) -> Result<SessionState> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decisions WHERE session_id = ? AND state IN ('open','sending','confirming')",
    )
    .bind(session_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(if pending > 0 {
        SessionState::WaitingDecision
    } else {
        SessionState::Running
    })
}

/// (제공자, 원본 세션 ID)의 세션을 찾고 없으면 유휴로 만든다. (세션 ID, 상태, 새로 만듦)
async fn ensure_session(
    conn: &mut SqliteConnection,
    ev: &ProviderEvent,
    now: i64,
) -> Result<(String, SessionState, bool)> {
    let title = match &ev.kind {
        EventKind::SessionStarted { title } if !title.trim().is_empty() => {
            title.trim().chars().take(MAX_TITLE).collect::<String>()
        }
        _ => ev.session_id.chars().take(12).collect(),
    };
    let inserted = sqlx::query(
        "INSERT INTO sessions (id, provider, provider_session_id, title, state, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'idle', ?, ?) ON CONFLICT (provider, provider_session_id) DO NOTHING",
    )
    .bind(new_id())
    .bind(&ev.provider)
    .bind(&ev.session_id)
    .bind(&title)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    let row = sqlx::query(
        "SELECT id, state FROM sessions WHERE provider = ? AND provider_session_id = ?",
    )
    .bind(&ev.provider)
    .bind(&ev.session_id)
    .fetch_one(&mut *conn)
    .await?;
    let state: String = row.try_get("state")?;
    Ok((
        row.try_get("id")?,
        SessionState::parse(&state).unwrap_or(SessionState::Idle),
        inserted,
    ))
}

async fn set_session(
    conn: &mut SqliteConnection,
    id: &str,
    state: SessionState,
    now: i64,
) -> Result<()> {
    sqlx::query("UPDATE sessions SET state = ?, updated_at = ? WHERE id = ?")
        .bind(state.as_str())
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn find_decision(
    conn: &mut SqliteConnection,
    session_id: &str,
    request_id: &str,
) -> Result<Option<DecisionRow>> {
    let row = sqlx::query(concat!(
        decision_select!(),
        " WHERE d.session_id = ? AND d.provider_request_id = ?"
    ))
    .bind(session_id)
    .bind(request_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(DecisionRow::from_row).transpose()
}

impl Service {
    /// 제공자 이벤트 하나를 반영한다. 같은 멱등 키는 한 번만 반영한다.
    pub async fn ingest(&self, ev: &ProviderEvent) -> Result<IngestResult, ServiceError> {
        if ev.provider.is_empty() || ev.session_id.is_empty() || ev.event_id.is_empty() {
            return Err(ServiceError::Invalid(
                "provider·session_id·event_id는 비울 수 없습니다".into(),
            ));
        }
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let fresh = sqlx::query(
            "INSERT INTO event_keys (provider, idem_key, received_at) VALUES (?, ?, ?) \
             ON CONFLICT (provider, idem_key) DO NOTHING",
        )
        .bind(&ev.provider)
        .bind(&ev.event_id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !fresh {
            tx.commit().await?;
            return Ok(IngestResult {
                duplicate: true,
                changed: false,
                new_decision: None,
            });
        }
        sqlx::query(
            "INSERT INTO providers (name, last_event_at) VALUES (?, ?) \
             ON CONFLICT (name) DO UPDATE SET last_event_at = excluded.last_event_at",
        )
        .bind(&ev.provider)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        // 세션과 무관한 이벤트: 하트비트·사용량
        match &ev.kind {
            EventKind::Heartbeat => {
                tx.commit().await?;
                return Ok(IngestResult {
                    duplicate: false,
                    changed: false,
                    new_decision: None,
                });
            }
            EventKind::UsageReported { windows } => {
                let changed = record_usage(&mut tx, &ev.provider, windows, now).await?;
                if changed {
                    bump_version(&mut tx).await?;
                }
                tx.commit().await?;
                return Ok(IngestResult {
                    duplicate: false,
                    changed,
                    new_decision: None,
                });
            }
            _ => {}
        }

        let (sid, state, created) = ensure_session(&mut tx, ev, now).await?;
        let mut changed = created;
        let mut new_decision = None;

        match &ev.kind {
            EventKind::Heartbeat | EventKind::UsageReported { .. } => unreachable!(),
            EventKind::SessionStarted { title } => {
                if !created && !title.trim().is_empty() {
                    let t: String = title.trim().chars().take(MAX_TITLE).collect();
                    sqlx::query("UPDATE sessions SET title = ?, updated_at = ? WHERE id = ?")
                        .bind(t)
                        .bind(now)
                        .bind(&sid)
                        .execute(&mut *tx)
                        .await?;
                    changed = true;
                }
            }
            EventKind::TurnStarted => {
                let next = active_state(&mut tx, &sid).await?;
                if state != next {
                    set_session(&mut tx, &sid, next, now).await?;
                    changed = true;
                }
            }
            EventKind::TurnEnded => {
                if !state.is_terminal() && state != SessionState::Idle {
                    set_session(&mut tx, &sid, SessionState::Idle, now).await?;
                    changed = true;
                }
            }
            EventKind::SubagentActivity => {
                // 상위 세션에 포함: 수는 그대로, 관측 시각만 갱신
                sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(&sid)
                    .execute(&mut *tx)
                    .await?;
            }
            EventKind::PermissionRequested { .. } => {
                if state != SessionState::WaitingPermission {
                    set_session(&mut tx, &sid, SessionState::WaitingPermission, now).await?;
                    changed = true;
                }
            }
            EventKind::PermissionResolved => {
                if state == SessionState::WaitingPermission {
                    let next = active_state(&mut tx, &sid).await?;
                    set_session(&mut tx, &sid, next, now).await?;
                    changed = true;
                }
            }
            EventKind::DecisionRequested {
                request_id,
                title,
                questions,
                allow_free_text,
                rationale,
                deadline_at,
            } => {
                validate_questions(questions, *allow_free_text).map_err(ServiceError::Invalid)?;
                if request_id.is_empty() || request_id.len() > 256 {
                    return Err(ServiceError::Invalid(
                        "request_id는 1~256자여야 합니다".into(),
                    ));
                }
                if rationale.as_ref().is_some_and(|r| r.len() > MAX_TEXT) {
                    return Err(ServiceError::Invalid("추천 이유가 너무 깁니다".into()));
                }
                let id = new_id();
                let title = title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| t.chars().take(MAX_TITLE).collect::<String>());
                let inserted = sqlx::query(
                    "INSERT INTO decisions (id, session_id, provider_request_id, title, questions, allow_free_text, \
                     rationale, created_at, deadline_at, state, closed_reason, version, updated_at) \
                     SELECT ?, ?, ?, COALESCE(?, s.title), ?, ?, ?, ?, ?, 'open', NULL, 1, ? FROM sessions s WHERE s.id = ? \
                     ON CONFLICT (session_id, provider_request_id) DO NOTHING",
                )
                .bind(&id)
                .bind(&sid)
                .bind(request_id)
                .bind(title)
                .bind(serde_json::to_string(questions).map_err(anyhow::Error::from)?)
                .bind(*allow_free_text as i64)
                .bind(rationale)
                .bind(now)
                .bind(deadline_at)
                .bind(now)
                .bind(&sid)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    > 0;
                if inserted {
                    set_session(&mut tx, &sid, SessionState::WaitingDecision, now).await?;
                    let version = bump_version(&mut tx).await?;
                    // 이미 마감이 지난 요청이면 푸시하지 않는다(다음 만료 처리에서 닫힘)
                    let expires = deadline_at
                        .unwrap_or(i64::MAX)
                        .min(now.saturating_add(PUSH_MAX_TTL_MS));
                    if expires > now {
                        sqlx::query(
                            "INSERT INTO push_outbox (device_id, kind, decision_id, snapshot_version, expires_at, \
                             state, attempts, next_attempt_at, created_at) \
                             SELECT d.id, 'decision', ?, ?, ?, 'pending', 0, ?, ? FROM devices d \
                             JOIN push_subscriptions p ON p.device_id = d.id WHERE d.revoked_at IS NULL",
                        )
                        .bind(&id)
                        .bind(version)
                        .bind(expires)
                        .bind(now)
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    }
                    new_decision = Some(id);
                    tx.commit().await?;
                    return Ok(IngestResult {
                        duplicate: false,
                        changed: true,
                        new_decision,
                    });
                }
            }
            EventKind::DecisionWithdrawn { request_id } => {
                if let Some(d) = find_decision(&mut tx, &sid, request_id).await?
                    && d.state.is_pending()
                {
                    finish_decision(
                        &mut tx,
                        &d,
                        DecisionState::Closed,
                        CloseReason::Withdrawn,
                        now,
                    )
                    .await?;
                    changed = true;
                }
            }
            EventKind::DecisionAnsweredElsewhere { request_id } => {
                if let Some(d) = find_decision(&mut tx, &sid, request_id).await?
                    && d.state.is_pending()
                {
                    finish_decision(
                        &mut tx,
                        &d,
                        DecisionState::Applied,
                        CloseReason::Elsewhere,
                        now,
                    )
                    .await?;
                    // 전달 중·확인 중이던 이 앱의 제출은 다른 곳의 답으로 대체됐다
                    sqlx::query(
                            "UPDATE submissions SET state = 'cancelled', detail = 'elsewhere', updated_at = ? \
                             WHERE decision_id = ? AND state IN ('delivering','unknown')",
                        )
                        .bind(now)
                        .bind(&d.id)
                        .execute(&mut *tx)
                        .await?;
                    changed = true;
                }
            }
            EventKind::SessionEnded { outcome } => {
                let rows = sqlx::query(concat!(
                    decision_select!(),
                    " WHERE d.session_id = ? AND d.state IN ('open','sending','confirming')"
                ))
                .bind(&sid)
                .fetch_all(&mut *tx)
                .await?;
                for r in &rows {
                    let d = DecisionRow::from_row(r)?;
                    finish_decision(
                        &mut tx,
                        &d,
                        DecisionState::Closed,
                        CloseReason::SessionEnded,
                        now,
                    )
                    .await?;
                }
                if state != outcome.state() {
                    set_session(&mut tx, &sid, outcome.state(), now).await?;
                }
                changed = true;
            }
        }
        if changed {
            bump_version(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(IngestResult {
            duplicate: false,
            changed,
            new_decision,
        })
    }
}
