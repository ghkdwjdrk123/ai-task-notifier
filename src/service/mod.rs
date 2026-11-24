//! 서비스 핵심: 상태 계산과 상태 전이. HTTP·CLI·작업자가 모두 이 계층을 거친다.
//!
//! 불변 조건
//! - 화면에 보이는 상태가 바뀌면 같은 트랜잭션에서 스냅샷 버전을 올린다.
//! - 새 결정과 그 푸시 발송 예약은 한 트랜잭션에서 기록한다.
//! - 제공자 호출(답 전달·상태 조회)과 푸시 발송은 트랜잭션 밖에서 한다.
//! - 반영 여부를 모르는 답은 자동으로 다시 보내지 않는다.

mod answers;
mod devices;
mod events;
mod outbox;
mod snapshot;

pub use answers::{SubmissionView, SubmitRequest};
pub use devices::DeviceView;
pub use events::IngestResult;
pub use outbox::OutboxStats;
pub use snapshot::{DecisionDetail, DecisionSummary, ProviderHealth, SessionView, Snapshot};

use crate::clock::Clock;
use crate::model::{CloseReason, DecisionState, Question, SessionState};
use crate::provider::ProviderClient;
use anyhow::{Context, Result};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// 제공자 이벤트가 이 시간(밀리초)보다 오래 없으면 '갱신 지연'으로 본다(설계 3장 측정 목표).
pub const STALE_AFTER_MS: i64 = 120_000;
/// 결정 푸시의 최대 유효 시간(마감이 없을 때).
pub const PUSH_MAX_TTL_MS: i64 = 24 * 3600 * 1000;
pub const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_CONFIRM_GIVE_UP_MS: i64 = 3600 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("찾을 수 없습니다")]
    NotFound,
    /// 요청 자체는 올바르지만 현재 상태와 맞지 않음(HTTP 409).
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    /// 입력이 올바르지 않음(HTTP 422).
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ServiceError {
    fn from(e: sqlx::Error) -> Self {
        ServiceError::Internal(e.into())
    }
}

pub(crate) fn conflict(code: &'static str, message: impl Into<String>) -> ServiceError {
    ServiceError::Conflict {
        code,
        message: message.into(),
    }
}

pub struct Service {
    pool: SqlitePool,
    clock: Arc<dyn Clock>,
    providers: HashMap<String, Arc<dyn ProviderClient>>,
    /// 제공자 호출(답 전달·상태 조회) 시간 제한. 넘기면 '알 수 없음'으로 본다.
    provider_timeout: Duration,
    /// '반영 확인 중'으로 이 시간(밀리초)보다 오래 남으면 '확인 불가'로 닫는다.
    confirm_give_up_ms: i64,
}

impl Service {
    pub fn new(pool: SqlitePool, clock: Arc<dyn Clock>) -> Self {
        Self {
            pool,
            clock,
            providers: HashMap::new(),
            provider_timeout: DEFAULT_PROVIDER_TIMEOUT,
            confirm_give_up_ms: DEFAULT_CONFIRM_GIVE_UP_MS,
        }
    }

    pub fn with_provider_timeout(mut self, t: Duration) -> Self {
        self.provider_timeout = t;
        self
    }

    pub fn with_confirm_give_up_ms(mut self, ms: i64) -> Self {
        self.confirm_give_up_ms = ms;
        self
    }

    /// 제공자 어댑터를 등록한다(이름은 이벤트의 provider와 같아야 함).
    pub fn with_provider(mut self, name: &str, client: Arc<dyn ProviderClient>) -> Self {
        self.providers.insert(name.to_string(), client);
        self
    }

    pub fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 스냅샷 버전을 1 올리고 새 값을 돌려준다.
pub(crate) async fn bump_version(conn: &mut SqliteConnection) -> Result<i64> {
    let v: i64 = sqlx::query_scalar(
        "UPDATE meta SET value = value + 1 WHERE key = 'snapshot_version' RETURNING value",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(v)
}

/// 결정과 그 세션 정보를 함께 읽는 SELECT(뒤에 WHERE를 붙여 쓴다).
macro_rules! decision_select {
    () => {
        "SELECT d.id, d.session_id, s.provider, s.provider_session_id, s.title AS session_title, \
         d.provider_request_id, d.title, d.questions, d.allow_free_text, d.rationale, d.created_at, \
         d.deadline_at, d.state, d.closed_reason, d.version, d.updated_at \
         FROM decisions d JOIN sessions s ON s.id = d.session_id"
    };
}
pub(crate) use decision_select;

#[derive(Debug, Clone)]
pub(crate) struct DecisionRow {
    pub id: String,
    pub session_id: String,
    pub provider: String,
    pub provider_session_id: String,
    pub session_title: String,
    pub provider_request_id: String,
    pub title: String,
    pub questions: Vec<Question>,
    pub allow_free_text: bool,
    pub rationale: Option<String>,
    pub created_at: i64,
    pub deadline_at: Option<i64>,
    pub state: DecisionState,
    pub closed_reason: Option<CloseReason>,
    pub version: i64,
}

impl DecisionRow {
    pub fn from_row(r: &SqliteRow) -> Result<Self> {
        let state: String = r.try_get("state")?;
        let reason: Option<String> = r.try_get("closed_reason")?;
        let questions: String = r.try_get("questions")?;
        Ok(Self {
            id: r.try_get("id")?,
            session_id: r.try_get("session_id")?,
            provider: r.try_get("provider")?,
            provider_session_id: r.try_get("provider_session_id")?,
            session_title: r.try_get("session_title")?,
            provider_request_id: r.try_get("provider_request_id")?,
            title: r.try_get("title")?,
            questions: serde_json::from_str(&questions).context("저장된 질문 JSON이 깨졌습니다")?,
            allow_free_text: r.try_get::<i64, _>("allow_free_text")? != 0,
            rationale: r.try_get("rationale")?,
            created_at: r.try_get("created_at")?,
            deadline_at: r.try_get("deadline_at")?,
            state: DecisionState::parse(&state).context("알 수 없는 결정 상태")?,
            closed_reason: reason.as_deref().and_then(CloseReason::parse),
            version: r.try_get("version")?,
        })
    }

    pub fn target(&self) -> crate::provider::DeliveryTarget {
        crate::provider::DeliveryTarget {
            provider_session_id: self.provider_session_id.clone(),
            provider_request_id: self.provider_request_id.clone(),
            questions: self.questions.clone(),
        }
    }
}

pub(crate) async fn load_decision(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<DecisionRow>> {
    let row = sqlx::query(concat!(decision_select!(), " WHERE d.id = ?"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(DecisionRow::from_row).transpose()
}

/// 결정의 상태를 바꾸고 버전을 올린다.
pub(crate) async fn set_decision_state(
    conn: &mut SqliteConnection,
    id: &str,
    state: DecisionState,
    reason: Option<CloseReason>,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE decisions SET state = ?, closed_reason = ?, version = version + 1, updated_at = ? WHERE id = ?",
    )
    .bind(state.as_str())
    .bind(reason.map(|r| r.as_str()))
    .bind(now)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 결정을 끝낸다(반영 완료 또는 철회·만료·세션 종료).
/// 아직 전달 전인 제출은 취소하고, 보내지 않은 푸시도 취소한다.
pub(crate) async fn finish_decision(
    conn: &mut SqliteConnection,
    d: &DecisionRow,
    state: DecisionState,
    reason: CloseReason,
    now: i64,
) -> Result<()> {
    debug_assert!(!state.is_pending());
    set_decision_state(conn, &d.id, state, Some(reason), now).await?;
    // 전달 전(queued)인 제출은 취소한다. 닫힌 결정이면 반영 여부를 모르던(unknown) 제출도 정리한다.
    let sql = if state == DecisionState::Closed {
        "UPDATE submissions SET state = 'cancelled', detail = ?, updated_at = ? \
         WHERE decision_id = ? AND state IN ('queued','unknown')"
    } else {
        "UPDATE submissions SET state = 'cancelled', detail = ?, updated_at = ? \
         WHERE decision_id = ? AND state = 'queued'"
    };
    sqlx::query(sql)
        .bind(reason.as_str())
        .bind(now)
        .bind(&d.id)
        .execute(&mut *conn)
        .await?;
    cancel_outbox_for_decision(conn, &d.id).await?;
    // 답이 반영되거나 요청이 철회·만료되면 원본 작업은 이어진다(설계 1.4).
    // 같은 세션에 다른 결정이 남아 있으면 계속 결정 대기다. 세션 종료·요청 소멸은 세션 상태를 따로 정한다.
    if !matches!(reason, CloseReason::SessionEnded | CloseReason::Gone) {
        let others: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM decisions WHERE session_id = ? AND state IN ('open','sending','confirming')",
        )
        .bind(&d.session_id)
        .fetch_one(&mut *conn)
        .await?;
        if others == 0 {
            sqlx::query("UPDATE sessions SET state = ?, updated_at = ? WHERE id = ? AND state = ?")
                .bind(SessionState::Running.as_str())
                .bind(now)
                .bind(&d.session_id)
                .bind(SessionState::WaitingDecision.as_str())
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

pub(crate) async fn cancel_outbox_for_decision(
    conn: &mut SqliteConnection,
    decision_id: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE push_outbox SET state = 'cancelled' WHERE decision_id = ? AND state = 'pending'",
    )
    .bind(decision_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 마감이 지난 결정을 만료시킨다. 서버 시각 ≥ 마감이면 만료(설계 2.3절).
/// 이미 원본에 전달 중이거나(delivering) 반영 확인 중인 결정은 건드리지 않는다.
pub(crate) async fn expire_due_in(conn: &mut SqliteConnection, now: i64) -> Result<usize> {
    let rows = sqlx::query(concat!(
        decision_select!(),
        " WHERE d.state IN ('open','sending') AND d.deadline_at IS NOT NULL AND d.deadline_at <= ? \
          AND NOT EXISTS (SELECT 1 FROM submissions x WHERE x.decision_id = d.id AND x.state = 'delivering')"
    ))
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    let mut n = 0;
    for r in &rows {
        let d = DecisionRow::from_row(r)?;
        finish_decision(conn, &d, DecisionState::Closed, CloseReason::Expired, now).await?;
        n += 1;
    }
    if n > 0 {
        bump_version(conn).await?;
    }
    Ok(n)
}

impl Service {
    /// 마감 처리. 작업자가 주기적으로 부르고, 조회·제출 직전에도 부른다.
    pub async fn expire_due(&self) -> Result<usize> {
        let mut tx = crate::db::begin(&self.pool).await?;
        let n = expire_due_in(&mut tx, self.now()).await?;
        tx.commit().await?;
        Ok(n)
    }
}
