//! 폰 화면용 조회: 스냅샷(요약·목록)과 결정 상세.

use super::answers::submission_from_row;
use super::{DecisionRow, STALE_AFTER_MS, Service, ServiceError, decision_select, load_decision};
use crate::clock::to_datetime;
use crate::model::{CloseReason, DecisionState, Question, SessionState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;

use super::SubmissionView;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// 이 값보다 작은 버전의 갱신은 화면에 반영하지 않는다(F09).
    pub version: i64,
    pub server_time: DateTime<Utc>,
    /// 진행 수: 작업 중인 최상위 세션 수.
    pub running: i64,
    /// 결정 대기 수: 열림·전송 중·반영 확인 중인 결정 수.
    pub waiting_decisions: i64,
    /// 지시 대기 수: 턴을 마치고 사용자의 다음 지시를 기다리는 세션 수.
    pub awaiting_input: i64,
    /// 제공자 하나라도 갱신이 늦으면 true(숫자 옆에 '갱신 지연' 표시).
    pub degraded: bool,
    pub providers: Vec<ProviderHealth>,
    pub sessions: Vec<SessionView>,
    pub decisions: Vec<DecisionSummary>,
    /// 제공자별 구독 사용량(마지막으로 보고된 값).
    pub usage: Vec<UsageView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageView {
    pub provider: String,
    /// 창 이름(예: "five_hour", "seven_day").
    pub window: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub status: Option<String>,
    /// 이 값을 받은 시각. 오래됐으면 화면에 'N분 전'으로 표시한다.
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderHealth {
    pub name: String,
    pub last_event_at: DateTime<Utc>,
    pub stale: bool,
    /// 사용량을 읽지 못하는 이유(예: 로그인 만료). 정상이면 없음.
    pub usage_error: Option<String>,
    /// 그 실패가 처음 난 시각.
    pub usage_error_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub provider: String,
    pub title: String,
    pub state: SessionState,
    pub updated_at: DateTime<Utc>,
    /// 세션을 처음 본 시각(같은 폴더 세션 구분용).
    pub created_at: DateTime<Utc>,
    /// 지금 상태가 된 시각('12분째 진행 중').
    pub state_since: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionSummary {
    pub id: String,
    pub provider: String,
    pub session_title: String,
    pub title: String,
    pub state: DecisionState,
    pub created_at: DateTime<Utc>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub version: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionDetail {
    #[serde(flatten)]
    pub summary: DecisionSummary,
    pub questions: Vec<Question>,
    pub allow_free_text: bool,
    pub rationale: Option<String>,
    pub closed_reason: Option<CloseReason>,
    /// 지금 답을 보낼 수 있는지.
    pub answerable: bool,
    pub latest_submission: Option<SubmissionView>,
}

fn summary(d: &DecisionRow) -> DecisionSummary {
    DecisionSummary {
        id: d.id.clone(),
        provider: d.provider.clone(),
        session_title: d.session_title.clone(),
        title: d.title.clone(),
        state: d.state,
        created_at: to_datetime(d.created_at),
        deadline_at: d.deadline_at.map(to_datetime),
        version: d.version,
    }
}

impl Service {
    /// 현재 스냅샷 버전만 읽는다(스트림이 변화를 싸게 감지하는 데 씀).
    pub async fn snapshot_version(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT value FROM meta WHERE key = 'snapshot_version'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        self.expire_due().await?;
        let now = self.now();
        // 숫자·목록·버전이 한 시점에서 읽히도록 한 트랜잭션에서 읽는다
        let mut tx = self.pool.begin().await?;
        let version: i64 =
            sqlx::query_scalar("SELECT value FROM meta WHERE key = 'snapshot_version'")
                .fetch_one(&mut *tx)
                .await?;
        let running: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE state = 'running'")
                .fetch_one(&mut *tx)
                .await?;
        let awaiting_input: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE state = 'awaiting_input'")
                .fetch_one(&mut *tx)
                .await?;
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM decisions WHERE state IN ('open','sending','confirming')",
        )
        .fetch_one(&mut *tx)
        .await?;
        let providers = sqlx::query(
            "SELECT name, last_event_at, usage_error, usage_error_at FROM providers ORDER BY name",
        )
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|r| {
                let last: i64 = r.try_get("last_event_at")?;
                Ok(ProviderHealth {
                    name: r.try_get("name")?,
                    last_event_at: to_datetime(last),
                    stale: now - last > STALE_AFTER_MS,
                    usage_error: r.try_get("usage_error")?,
                    usage_error_since: r
                        .try_get::<Option<i64>, _>("usage_error_at")?
                        .map(to_datetime),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let sessions = sqlx::query(
            "SELECT id, provider, title, state, updated_at, created_at, \
             COALESCE(state_since, updated_at) AS state_since FROM sessions \
             WHERE state NOT IN ('completed','interrupted','failed') ORDER BY created_at",
        )
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| {
            let state: String = r.try_get("state")?;
            Ok(SessionView {
                id: r.try_get("id")?,
                provider: r.try_get("provider")?,
                title: r.try_get("title")?,
                state: SessionState::parse(&state).context("알 수 없는 세션 상태")?,
                updated_at: to_datetime(r.try_get("updated_at")?),
                created_at: to_datetime(r.try_get("created_at")?),
                state_since: to_datetime(r.try_get("state_since")?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
        // 마감 임박순, 마감이 같거나 없으면 생성순(F06)
        let decisions = sqlx::query(concat!(
            decision_select!(),
            " WHERE d.state IN ('open','sending','confirming') \
              ORDER BY d.deadline_at IS NULL, d.deadline_at, d.created_at, d.id"
        ))
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| DecisionRow::from_row(r).map(|d| summary(&d)))
        .collect::<Result<Vec<_>>>()?;
        let usage = sqlx::query(
            "SELECT provider, window, used_percent, resets_at, status, observed_at FROM usage_windows \
             ORDER BY provider, window",
        )
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|r| {
            Ok(UsageView {
                provider: r.try_get("provider")?,
                window: r.try_get("window")?,
                used_percent: r.try_get("used_percent")?,
                resets_at: r.try_get::<Option<i64>, _>("resets_at")?.map(to_datetime),
                status: r.try_get("status")?,
                observed_at: to_datetime(r.try_get("observed_at")?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(Snapshot {
            usage,
            version,
            server_time: to_datetime(now),
            running,
            waiting_decisions: waiting,
            awaiting_input,
            degraded: providers.iter().any(|p| p.stale),
            providers,
            sessions,
            decisions,
        })
    }

    pub async fn decision_detail(&self, id: &str) -> Result<DecisionDetail, ServiceError> {
        self.expire_due().await?;
        let mut conn = self.pool.acquire().await?;
        let d = load_decision(&mut conn, id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let latest = sqlx::query(
            "SELECT id, decision_id, state, detail, answers, created_at, updated_at FROM submissions \
             WHERE decision_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let latest_submission = latest.as_ref().map(submission_from_row).transpose()?;
        Ok(DecisionDetail {
            summary: summary(&d),
            answerable: d.state == DecisionState::Open,
            questions: d.questions,
            allow_free_text: d.allow_free_text,
            rationale: d.rationale,
            closed_reason: d.closed_reason,
            latest_submission,
        })
    }
}
