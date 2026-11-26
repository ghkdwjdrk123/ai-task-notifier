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
mod snapshot;

pub use answers::{SubmissionView, SubmitRequest};
pub use devices::DeviceView;
pub use events::IngestResult;
pub use snapshot::{
    DecisionDetail, DecisionSummary, ProviderHealth, SessionView, Snapshot, UsageView,
};

use crate::clock::Clock;
use crate::model::{CloseReason, DecisionState, Question, SessionState};
use crate::provider::ProviderClient;
use anyhow::{Context, Result};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// 제공자 이벤트가 이 시간(밀리초)보다 오래 없으면 '갱신 지연'으로 본다.
/// 수집기가 사용량을 5분마다, Codex 감시가 1분마다 신호를 보내므로 15분.
pub const STALE_AFTER_MS: i64 = 15 * 60_000;
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

    /// 같은 DB를 쓰는 제공자 어댑터(우편함 등)가 쓴다.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
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

/// 세션 요약(훅이 쓰기 전에 확인하는 용도).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBrief {
    pub state: SessionState,
    pub updated_at: i64,
    pub title: String,
}

/// 진행 중·대기 상태로 이 시간(밀리초) 넘게 소식이 없으면 유휴로 본다(종료 신호를 놓친 세션).
pub const SWEEP_ACTIVE_AFTER_MS: i64 = 3 * 3600 * 1000;
/// 유휴로 이 시간 넘게 소식이 없으면 끝난 세션으로 보고 목록에서 뺀다.
pub const SWEEP_IDLE_AFTER_MS: i64 = 24 * 3600 * 1000;
/// 끝난 결정(질문·실행 내용·답)을 두는 기간.
pub const DECISION_RETAIN_MS: i64 = 24 * 3600 * 1000;
/// 끝난 세션(제목·상태)을 두는 기간.
pub const SESSION_RETAIN_MS: i64 = 7 * 24 * 3600 * 1000;

impl Service {
    /// 세션의 현재 상태·마지막 관측 시각·제목. 없으면 None.
    pub async fn session_brief(
        &self,
        provider: &str,
        provider_session_id: &str,
    ) -> Result<Option<SessionBrief>> {
        let row = sqlx::query(
            "SELECT state, updated_at, title FROM sessions WHERE provider = ? AND provider_session_id = ?",
        )
        .bind(provider)
        .bind(provider_session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(r) => {
                let s: String = r.try_get("state")?;
                SessionState::parse(&s).map(|state| SessionBrief {
                    state,
                    updated_at: r.try_get("updated_at").unwrap_or(0),
                    title: r.try_get("title").unwrap_or_default(),
                })
            }
            None => None,
        })
    }

    /// 수집기가 살아 있다는 신호: 제공자의 마지막 이벤트 시각만 고친다(멱등 키를 쌓지 않음).
    pub async fn heartbeat(&self, provider: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO providers (name, last_event_at) VALUES (?, ?) \
             ON CONFLICT (name) DO UPDATE SET last_event_at = excluded.last_event_at",
        )
        .bind(provider)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 지난 기록 정리(대화 내용을 오래 두지 않는다).
    /// - 끝난 결정(반영·닫힘)은 `DECISION_RETAIN_MS` 뒤 질문·실행 내용·답과 함께 지운다.
    /// - 끝난 세션(완료·중단·실패)은 남은 결정이 없으면 `SESSION_RETAIN_MS` 뒤 지운다.
    /// - 다 쓴·만료된 등록 코드는 하루 뒤 지운다. 지운 결정·세션 수.
    pub async fn purge_old_records(&self) -> Result<usize> {
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let cutoff = now - DECISION_RETAIN_MS;
        // 결정에 딸린 제출을 먼저 지운다(외래 키)
        sqlx::query(
            "DELETE FROM submissions WHERE decision_id IN \
             (SELECT id FROM decisions WHERE state IN ('applied','closed') AND updated_at < ?)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        let d = sqlx::query(
            "DELETE FROM decisions WHERE state IN ('applied','closed') AND updated_at < ?",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let s = sqlx::query(
            "DELETE FROM sessions WHERE state IN ('completed','interrupted','failed') AND updated_at < ? \
             AND NOT EXISTS (SELECT 1 FROM decisions WHERE decisions.session_id = sessions.id)",
        )
        .bind(now - SESSION_RETAIN_MS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM pairing_codes WHERE (used_at IS NOT NULL OR expires_at <= ?) AND created_at < ?",
        )
        .bind(now)
        .bind(now - 24 * 3600 * 1000)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((d + s) as usize)
    }

    /// 종료 신호를 놓친 세션 정리: 오래 조용한 진행·대기 세션은 유휴로, 오래된 유휴 세션은 완료로.
    /// 열린 결정이 있는 세션은 건드리지 않는다. 바꾼 세션 수.
    pub async fn sweep_sessions(&self) -> Result<usize> {
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let a = sqlx::query(
            "UPDATE sessions SET state = 'idle', updated_at = ? \
             WHERE state IN ('running','waiting_permission') AND updated_at < ? \
             AND NOT EXISTS (SELECT 1 FROM decisions d WHERE d.session_id = sessions.id \
                             AND d.state IN ('open','sending','confirming'))",
        )
        .bind(now)
        .bind(now - SWEEP_ACTIVE_AFTER_MS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let b = sqlx::query(
            "UPDATE sessions SET state = 'completed', updated_at = ? WHERE state IN ('idle','awaiting_input') AND updated_at < ?",
        )
        .bind(now)
        .bind(now - SWEEP_IDLE_AFTER_MS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if a + b > 0 {
            bump_version(&mut tx).await?;
        }
        // 멱등 키는 7일 지나면 지운다(재전송은 길어야 몇 분 안에 온다)
        sqlx::query("DELETE FROM event_keys WHERE received_at < ?")
            .bind(now - 7 * 24 * 3600 * 1000)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((a + b) as usize)
    }
}
