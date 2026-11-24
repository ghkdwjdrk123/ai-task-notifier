//! 답변 접수·전달·반영 확인.
//!
//! 흐름: 접수(queued, 결정은 sending) → 전달 시작(delivering) → 제공자 결과
//!   - 수락: 제출 applied, 결정 applied
//!   - 미반영 명시: 제출 rejected, 결정 open(다시 답할 수 있음)
//!   - 알 수 없음: 제출 unknown, 결정 confirming(자동 재전송 없음, 원본 조회로만 정리)

use super::{
    DecisionRow, Service, ServiceError, bump_version, conflict, decision_select, expire_due_in,
    finish_decision, load_decision, set_decision_state,
};
use crate::clock::to_datetime;
use crate::model::{
    AnswerValue, Answers, CloseReason, DecisionState, DeliveryOutcome, Question, RequestStatus,
    SubmissionState,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use std::collections::HashSet;

const MAX_ANSWER: usize = 4000;

#[derive(Debug, Clone, Deserialize)]
pub struct SubmitRequest {
    /// 폰이 만든 제출 ID. 같은 ID로 다시 보내면 기존 결과를 돌려준다.
    pub submission_id: String,
    /// 폰이 보고 있던 결정 버전. 그 사이 상태가 바뀌었으면 거절한다.
    pub decision_version: i64,
    pub answers: Answers,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubmissionView {
    pub id: String,
    pub decision_id: String,
    pub state: SubmissionState,
    pub detail: Option<String>,
    pub answers: Answers,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) fn submission_from_row(r: &SqliteRow) -> Result<SubmissionView> {
    let state: String = r.try_get("state")?;
    let answers: String = r.try_get("answers")?;
    Ok(SubmissionView {
        id: r.try_get("id")?,
        decision_id: r.try_get("decision_id")?,
        state: SubmissionState::parse(&state).context("알 수 없는 제출 상태")?,
        detail: r.try_get("detail")?,
        answers: serde_json::from_str(&answers)?,
        created_at: to_datetime(r.try_get("created_at")?),
        updated_at: to_datetime(r.try_get("updated_at")?),
    })
}

macro_rules! submission_select {
    () => {
        "SELECT id, decision_id, state, detail, answers, created_at, updated_at, decision_version FROM submissions"
    };
}

fn valid_submission_id(s: &str) -> bool {
    (8..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 답이 질문과 맞는지 검사한다. 모든 질문에 답해야 하고 모르는 질문 키는 받지 않는다.
pub(crate) fn validate_answers(
    qs: &[Question],
    allow_free_text: bool,
    answers: &Answers,
) -> Result<(), String> {
    let known: HashSet<&str> = qs.iter().map(|q| q.question.as_str()).collect();
    if let Some(k) = answers.keys().find(|k| !known.contains(k.as_str())) {
        return Err(format!("없는 질문에 대한 답입니다: {k}"));
    }
    for q in qs {
        let labels: HashSet<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        let Some(v) = answers.get(&q.question) else {
            return Err(format!("답하지 않은 질문이 있습니다: {}", q.question));
        };
        match v {
            AnswerValue::One(s) => {
                let t = s.trim();
                if t.is_empty() || s.len() > MAX_ANSWER {
                    return Err("답이 비었거나 너무 깁니다".into());
                }
                if !labels.contains(s.as_str()) && !allow_free_text {
                    return Err(format!("선택지에 없는 답입니다: {s}"));
                }
            }
            AnswerValue::Many(list) => {
                if !q.multi_select {
                    return Err("여러 개를 고를 수 없는 질문입니다".into());
                }
                let mut seen = HashSet::new();
                if list.is_empty()
                    || list
                        .iter()
                        .any(|s| !labels.contains(s.as_str()) || !seen.insert(s))
                {
                    return Err("다중 선택 답이 비었거나 선택지에 없거나 겹칩니다".into());
                }
            }
        }
    }
    Ok(())
}

async fn load_submission(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<(SubmissionView, i64)>> {
    let q = sqlx::query(concat!(submission_select!(), " WHERE id = ?"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    q.as_ref()
        .map(|r| {
            Ok((
                submission_from_row(r)?,
                r.try_get::<i64, _>("decision_version")?,
            ))
        })
        .transpose()
}

async fn set_submission(
    conn: &mut SqliteConnection,
    id: &str,
    state: SubmissionState,
    detail: Option<&str>,
    now: i64,
) -> Result<()> {
    sqlx::query("UPDATE submissions SET state = ?, detail = ?, updated_at = ? WHERE id = ?")
        .bind(state.as_str())
        .bind(detail)
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

impl Service {
    /// 답을 접수한다. (제출, 새로 만듦) — 같은 ID·같은 입력의 재전송이면 기존 제출과 false.
    pub async fn submit(
        &self,
        device_id: Option<&str>,
        decision_id: &str,
        req: &SubmitRequest,
    ) -> Result<(SubmissionView, bool), ServiceError> {
        if !valid_submission_id(&req.submission_id) {
            return Err(ServiceError::Invalid(
                "submission_id는 8~64자 영문·숫자·-·_여야 합니다".into(),
            ));
        }
        let now = self.now();
        let answers_json = serde_json::to_string(&req.answers).map_err(anyhow::Error::from)?;
        let mut tx = crate::db::begin(&self.pool).await?;
        if expire_due_in(&mut tx, now).await? > 0 {
            // 아래에서 오류를 돌려줘도 만료 처리는 남도록 먼저 저장한다
            tx.commit().await?;
            tx = crate::db::begin(&self.pool).await?;
        }

        if let Some((existing, version)) = load_submission(&mut tx, &req.submission_id).await? {
            tx.commit().await?;
            if existing.decision_id == decision_id
                && existing.answers == req.answers
                && version == req.decision_version
            {
                return Ok((existing, false));
            }
            return Err(conflict(
                "submission_id_reused",
                "같은 제출 ID로 다른 내용을 보냈습니다",
            ));
        }

        let Some(d) = load_decision(&mut tx, decision_id).await? else {
            return Err(ServiceError::NotFound);
        };
        if d.state != DecisionState::Open {
            let msg = match (d.state, d.closed_reason) {
                (DecisionState::Closed, Some(CloseReason::Expired)) => {
                    "마감이 지나 답할 수 없습니다"
                }
                (DecisionState::Closed, _) => "이미 닫힌 요청입니다",
                (DecisionState::Applied, _) => "이미 처리된 요청입니다",
                _ => "다른 답을 전송·확인 중입니다",
            };
            return Err(conflict("not_open", msg));
        }
        if d.version != req.decision_version {
            return Err(conflict(
                "stale_version",
                format!("요청이 바뀌었습니다(현재 버전 {})", d.version),
            ));
        }
        validate_answers(&d.questions, d.allow_free_text, &req.answers)
            .map_err(ServiceError::Invalid)?;

        sqlx::query(
            "INSERT INTO submissions (id, decision_id, decision_version, answers, device_id, state, detail, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'queued', NULL, ?, ?)",
        )
        .bind(&req.submission_id)
        .bind(&d.id)
        .bind(d.version)
        .bind(&answers_json)
        .bind(device_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        set_decision_state(&mut tx, &d.id, DecisionState::Sending, None, now).await?;
        bump_version(&mut tx).await?;
        let (view, _) = load_submission(&mut tx, &req.submission_id)
            .await?
            .context("방금 넣은 제출이 없습니다")?;
        tx.commit().await?;
        Ok((view, true))
    }

    pub async fn get_submission(
        &self,
        decision_id: &str,
        submission_id: &str,
    ) -> Result<SubmissionView, ServiceError> {
        let mut conn = self.pool.acquire().await?;
        match load_submission(&mut conn, submission_id).await? {
            Some((s, _)) if s.decision_id == decision_id => Ok(s),
            _ => Err(ServiceError::NotFound),
        }
    }

    /// 접수된 답을 차례로 원본에 전달한다. 전달을 시작한 건수를 돌려준다.
    pub async fn deliver_pending(&self) -> Result<usize> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM submissions WHERE state = 'queued' ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut n = 0;
        for id in ids {
            if self.deliver_one(&id).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// 제출 하나를 전달한다. 전달 직전에 마감·철회를 다시 확인한다(설계 T20).
    async fn deliver_one(&self, submission_id: &str) -> Result<bool> {
        // 1) 전달 권한 확보: queued → delivering
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        expire_due_in(&mut tx, now).await?;
        let Some((sub, _)) = load_submission(&mut tx, submission_id).await? else {
            return Ok(false);
        };
        if sub.state != SubmissionState::Queued {
            // 만료 처리 등으로 이미 취소됨
            tx.commit().await?;
            return Ok(false);
        }
        let d = load_decision(&mut tx, &sub.decision_id)
            .await?
            .context("제출의 결정이 없습니다")?;
        if d.state != DecisionState::Sending {
            set_submission(
                &mut tx,
                submission_id,
                SubmissionState::Cancelled,
                Some("not_sending"),
                now,
            )
            .await?;
            bump_version(&mut tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let provider = self.providers.get(&d.provider).cloned();
        if provider.is_none() {
            // 직접 응답 경로가 없는 제공자: 미반영으로 되돌려 사용자가 공식 경로로 답하게 한다
            set_submission(
                &mut tx,
                submission_id,
                SubmissionState::Rejected,
                Some("no_provider"),
                now,
            )
            .await?;
            set_decision_state(&mut tx, &d.id, DecisionState::Open, None, now).await?;
            bump_version(&mut tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        set_submission(
            &mut tx,
            submission_id,
            SubmissionState::Delivering,
            None,
            now,
        )
        .await?;
        tx.commit().await?;

        // 2) 트랜잭션 밖에서 원본에 전달. 시간 제한을 넘기면 반영 여부를 알 수 없는 것으로 본다.
        let outcome = match tokio::time::timeout(
            self.provider_timeout,
            provider.unwrap().deliver_answer(&d.target(), &sub.answers),
        )
        .await
        {
            Ok(o) => o,
            Err(_) => DeliveryOutcome::Unknown("timeout".into()),
        };

        // 3) 결과 기록. 전달하는 동안 다른 경로(다른 곳에서 반영 등)로 제출이 이미 정리됐으면
        //    덮어쓰지 않고 결정도 건드리지 않는다.
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let cur = load_decision(&mut tx, &d.id)
            .await?
            .context("결정이 사라졌습니다")?;
        let (sub_state, detail) = match &outcome {
            DeliveryOutcome::Accepted => (SubmissionState::Applied, None),
            DeliveryOutcome::NotApplied(r) => (SubmissionState::Rejected, Some(r.as_str())),
            DeliveryOutcome::Unknown(r) if cur.state.is_pending() => {
                (SubmissionState::Unknown, Some(r.as_str()))
            }
            // 결정이 이미 닫혔으면 확인할 길이 없으므로 바로 종결한다
            DeliveryOutcome::Unknown(_) => (SubmissionState::Cancelled, Some("decision_closed")),
        };
        let claimed = sqlx::query(
            "UPDATE submissions SET state = ?, detail = ?, updated_at = ? WHERE id = ? AND state = 'delivering'",
        )
        .bind(sub_state.as_str())
        .bind(detail)
        .bind(now)
        .bind(submission_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if claimed {
            match outcome {
                DeliveryOutcome::Accepted if cur.state.is_pending() => {
                    finish_decision(
                        &mut tx,
                        &cur,
                        DecisionState::Applied,
                        CloseReason::ThisApp,
                        now,
                    )
                    .await?;
                }
                DeliveryOutcome::NotApplied(_) if cur.state == DecisionState::Sending => {
                    set_decision_state(&mut tx, &cur.id, DecisionState::Open, None, now).await?;
                }
                DeliveryOutcome::Unknown(_) if cur.state == DecisionState::Sending => {
                    set_decision_state(&mut tx, &cur.id, DecisionState::Confirming, None, now)
                        .await?;
                }
                _ => {}
            }
        }
        bump_version(&mut tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// '반영 확인 중' 결정을 원본 상태로 정리한다. 정리한 건수를 돌려준다.
    pub async fn reconcile_confirming(&self) -> Result<usize> {
        let mut n = self.give_up_unconfirmed().await?;
        let rows = sqlx::query(concat!(decision_select!(), " WHERE d.state = 'confirming'"))
            .fetch_all(&self.pool)
            .await?;
        let decisions = rows
            .iter()
            .map(DecisionRow::from_row)
            .collect::<Result<Vec<_>>>()?;
        for d in decisions {
            let Some(p) = self.providers.get(&d.provider) else {
                continue;
            };
            let status = tokio::time::timeout(self.provider_timeout, p.request_status(&d.target()))
                .await
                .unwrap_or(RequestStatus::Unknown);
            if status == RequestStatus::Unknown {
                continue;
            }
            let now = self.now();
            let mut tx = crate::db::begin(&self.pool).await?;
            let Some(cur) = load_decision(&mut tx, &d.id).await? else {
                continue;
            };
            if cur.state != DecisionState::Confirming {
                continue; // 그 사이 다른 경로로 정리됨
            }
            let sub_state = match status {
                RequestStatus::Answered => {
                    finish_decision(
                        &mut tx,
                        &cur,
                        DecisionState::Applied,
                        CloseReason::ThisApp,
                        now,
                    )
                    .await?;
                    (SubmissionState::Applied, None)
                }
                RequestStatus::Open => {
                    // 원본이 아직 기다림 = 우리 답은 반영되지 않음 → 사용자가 다시 보낼 수 있다
                    set_decision_state(&mut tx, &cur.id, DecisionState::Open, None, now).await?;
                    (SubmissionState::Rejected, Some("not_applied_confirmed"))
                }
                RequestStatus::Gone => {
                    finish_decision(&mut tx, &cur, DecisionState::Closed, CloseReason::Gone, now)
                        .await?;
                    (SubmissionState::Cancelled, Some("gone"))
                }
                RequestStatus::Unknown => unreachable!(),
            };
            sqlx::query("UPDATE submissions SET state = ?, detail = ?, updated_at = ? WHERE decision_id = ? AND state = 'unknown'")
                .bind(sub_state.0.as_str())
                .bind(sub_state.1)
                .bind(now)
                .bind(&cur.id)
                .execute(&mut *tx)
                .await?;
            bump_version(&mut tx).await?;
            tx.commit().await?;
            n += 1;
        }
        Ok(n)
    }

    /// 반영 확인 중으로 너무 오래(기본 1시간) 남은 결정은 '확인 불가'로 닫는다.
    /// 제공자가 연결되지 않았거나 계속 상태를 모를 때 대기 수에 영원히 남지 않게 한다.
    async fn give_up_unconfirmed(&self) -> Result<usize> {
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let rows = sqlx::query(concat!(
            decision_select!(),
            " WHERE d.state = 'confirming' AND d.updated_at <= ?"
        ))
        .bind(now - self.confirm_give_up_ms)
        .fetch_all(&mut *tx)
        .await?;
        for r in &rows {
            let d = DecisionRow::from_row(r)?;
            finish_decision(
                &mut tx,
                &d,
                DecisionState::Closed,
                CloseReason::Unconfirmed,
                now,
            )
            .await?;
        }
        if !rows.is_empty() {
            bump_version(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }

    /// 재시작 직후: 전달 중에 멈춘 제출은 반영 여부를 알 수 없으므로 '반영 확인 중'으로 돌린다.
    pub async fn recover_after_restart(&self) -> Result<usize> {
        let now = self.now();
        let mut tx = crate::db::begin(&self.pool).await?;
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT decision_id FROM submissions WHERE state = 'delivering'")
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("UPDATE submissions SET state = 'unknown', detail = 'restart', updated_at = ? WHERE state = 'delivering'")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        for id in &ids {
            sqlx::query(
                "UPDATE decisions SET state = 'confirming', version = version + 1, updated_at = ? WHERE id = ? AND state = 'sending'",
            )
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        if !ids.is_empty() {
            bump_version(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(ids.len())
    }
}
