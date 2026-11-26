//! 폰에서 보낸 답을 실행 중인 Claude Code 세션에 넣는다.
//!
//! 경로(2c단계, 설계서 6장 '2c단계')
//! 1. Claude Code가 권한·질문 창을 띄우면 `PermissionRequest` 훅(`notifier hook claude`)이 불린다.
//!    맥 화면의 창은 훅과 **동시에** 떠 있고, 먼저 온 답이 쓰인다(대화형 TUI에서 실측).
//! 2. 훅 프로세스는 결정 요청을 만들고 우편함(`hook_waits`) 행을 만든 뒤 기다린다.
//! 3. 폰이 답하면 서버의 [`ClaudeHookProvider`]가 우편함에 답을 쓰고, 훅이 가져가(`taken_at`)
//!    훅 출력(JSON)으로 Claude Code에 돌려준다.
//! 4. 맥에서 먼저 답하면 Claude Code는 훅을 끝내지 않고 출력만 버린다. 그래서 같은 세션의 뒤따르는
//!    훅(PostToolUse·Stop 등)이 오면 우편함을 닫고(`gave_up_at`) 결정을 '다른 곳에서 답함'으로 닫는다.
//!    기다리던 훅은 결정이 닫힌 것을 보고 아무것도 출력하지 않고 끝난다.

use crate::clock::Clock;
use crate::collect::CLAUDE;
use crate::model::{
    AnswerValue, Answers, DeliveryOutcome, EventKind, ProviderEvent, Question, QuestionOption,
    RequestStatus,
};
use crate::provider::{DeliveryTarget, ProviderClient};
use crate::service::Service;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::sync::Arc;
use std::time::Duration;

/// Claude Code의 질문 도구 이름.
pub const ASK_TOOL: &str = "AskUserQuestion";
pub const ALLOW: &str = "허용";
pub const DENY: &str = "거부";
/// 훅이 우편함을 살피는 간격.
const POLL: Duration = Duration::from_millis(300);
/// 서버가 답을 넣은 뒤 훅이 가져가기를 기다리는 최대 시간. 넘기면 '반영 확인 중'.
const TAKE_WAIT: Duration = Duration::from_secs(10);
/// 훅이 살아 있음을 적는 간격.
const BEAT: Duration = Duration::from_secs(5);
/// 이만큼 살아 있다는 표시가 없으면 훅이 사라진 것으로 본다(밀리초).
pub const HOOK_DEAD_MS: i64 = 15_000;
/// 연속 DB 오류가 이만큼이면 기다림을 포기한다(약 6초).
const MAX_POLL_ERRORS: u32 = 20;
/// 같은 에이전트의 다른 도구가 끝나면 맥에서 이미 답한 것으로 보는데, 창이 생긴 직후 끝난
/// 병렬 도구(예: 함께 부른 Read)는 제외하려고 두는 여유(밀리초).
const PARALLEL_GRACE_MS: i64 = 3_000;
/// 권한 요약(추천 이유 칸) 최대 글자 수.
const MAX_SUMMARY: usize = 3000;

/// 훅이 기다리는 기본 시간(초). Claude Code 훅 기본 시간 제한(600초)보다 짧게.
pub const DEFAULT_WAIT_SECS: u64 = 570;

/// tool_input에서 answers를 뺀 정규 JSON의 sha256. 맥에서 답한 뒤 오는 PostToolUse와 짝을 맞춘다.
pub fn input_hash(tool_input: &Value) -> String {
    let mut v = tool_input.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("answers");
    }
    hex::encode(Sha256::digest(serde_json::to_vec(&v).unwrap_or_default()))
}

/// 폰에 보일 결정 요청 하나.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    pub tool_name: String,
    pub tool_input: Value,
    pub questions: Vec<Question>,
    pub allow_free_text: bool,
    pub rationale: Option<String>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// 도구 입력을 사람이 읽을 요약으로. 폰의 '추천 이유' 칸에 보인다.
fn tool_summary(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let text = match tool {
        "Bash" => {
            let desc = s("description");
            if desc.is_empty() {
                format!("$ {}", s("command"))
            } else {
                format!("{desc}\n\n$ {}", s("command"))
            }
        }
        "Edit" | "MultiEdit" | "Write" | "Read" => format!("파일: {}", s("file_path")),
        "NotebookEdit" => format!("노트북: {}", s("notebook_path")),
        "WebFetch" => format!("주소: {}", s("url")),
        "WebSearch" => format!("검색: {}", s("query")),
        _ => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    truncate(&text, MAX_SUMMARY)
}

/// PermissionRequest 훅 입력 → 폰에 보일 결정 요청. 답할 수 없는 모양이면 None.
pub fn permission_ask(input: &Value) -> Option<PermissionAsk> {
    let tool_name = input.get("tool_name")?.as_str()?.to_string();
    let tool_input = input.get("tool_input").cloned().unwrap_or(json!({}));
    if tool_name == ASK_TOOL {
        let questions: Vec<Question> =
            serde_json::from_value(tool_input.get("questions")?.clone()).ok()?;
        if questions.is_empty() {
            return None;
        }
        return Some(PermissionAsk {
            tool_name,
            tool_input,
            questions,
            // Claude Code 질문 창에는 '직접 입력'이 있다
            allow_free_text: true,
            rationale: None,
        });
    }
    let q = Question {
        question: format!("{tool_name} 실행을 허용할까요?"),
        header: Some("권한".into()),
        options: vec![
            QuestionOption {
                label: ALLOW.into(),
                description: Some("이번 한 번만 허용".into()),
            },
            QuestionOption {
                label: DENY.into(),
                description: Some("실행하지 않고 Claude에게 거부를 알림".into()),
            },
        ],
        multi_select: false,
    };
    let rationale = tool_summary(&tool_name, &tool_input);
    Some(PermissionAsk {
        tool_name,
        tool_input,
        questions: vec![q],
        allow_free_text: false,
        rationale: Some(rationale),
    })
}

/// 폰 답 → PermissionRequest 훅 출력.
pub fn hook_output(ask: &PermissionAsk, answers: &Answers) -> Value {
    let decision = if ask.tool_name == ASK_TOOL {
        // Claude Code 형식: {질문 전문: 답 문자열}. 다중 선택은 쉼표로 잇는다.
        let flat: serde_json::Map<String, Value> = answers
            .iter()
            .map(|(q, v)| {
                let s = match v {
                    AnswerValue::One(s) => s.clone(),
                    AnswerValue::Many(list) => list.join(", "),
                };
                (q.clone(), Value::String(s))
            })
            .collect();
        let mut updated = ask.tool_input.clone();
        if let Some(o) = updated.as_object_mut() {
            o.insert("answers".into(), Value::Object(flat));
        }
        json!({"behavior": "allow", "updatedInput": updated})
    } else {
        let allowed = answers
            .values()
            .next()
            .is_some_and(|v| *v == AnswerValue::One(ALLOW.into()));
        if allowed {
            json!({"behavior": "allow"})
        } else {
            json!({"behavior": "deny", "message": "사용자가 폰에서 거부했습니다."})
        }
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
}

/// 하위 에이전트 안의 훅이면 그 agent_id, 주 대화면 빈 문자열.
fn agent_id(input: &Value) -> &str {
    input.get("agent_id").and_then(Value::as_str).unwrap_or("")
}

async fn ingest(svc: &Service, sid: &str, kind: EventKind) -> Result<()> {
    let ev = ProviderEvent {
        provider: CLAUDE.into(),
        session_id: sid.into(),
        event_id: format!("hook:{}", uuid::Uuid::new_v4().simple()),
        kind,
    };
    svc.ingest(&ev).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// 우편함을 닫는다(아직 가져가지 않았을 때만). 닫았으면 true.
async fn give_up(pool: &SqlitePool, request_id: &str, now: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE hook_waits SET gave_up_at = ? WHERE request_id = ? AND taken_at IS NULL AND gave_up_at IS NULL",
    )
    .bind(now)
    .bind(request_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// 폰 답이 들어와 있고 결정이 아직 진행 중이면 가져간다.
async fn try_take(pool: &SqlitePool, request_id: &str, now: i64) -> Result<Option<Answers>> {
    let row: Option<String> = sqlx::query_scalar(
        "UPDATE hook_waits SET taken_at = ? \
         WHERE request_id = ? AND answers IS NOT NULL AND taken_at IS NULL AND gave_up_at IS NULL \
           AND EXISTS (SELECT 1 FROM decisions d JOIN sessions s ON s.id = d.session_id \
                       WHERE s.provider = ? AND s.provider_session_id = hook_waits.provider_session_id \
                         AND d.provider_request_id = hook_waits.request_id \
                         AND d.state IN ('open','sending','confirming')) \
         RETURNING answers",
    )
    .bind(now)
    .bind(request_id)
    .bind(CLAUDE)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    })
}

/// `PermissionRequest` 훅: 결정 요청을 만들고 폰 답을 기다린다.
/// 폰 답이 오면 훅 출력(JSON)을, 맥에서 먼저 답했거나 시간이 다 되면 None을 돌려준다.
pub async fn run_permission_hook(
    svc: &Service,
    input: &Value,
    wait: Duration,
) -> Result<Option<Value>> {
    let Some(sid) = input.get("session_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(ask) = permission_ask(input) else {
        return Ok(None);
    };
    let pool = svc.pool();
    let request_id = format!("perm:{}", uuid::Uuid::new_v4().simple());
    let now = svc.now();
    sqlx::query(
        "INSERT INTO hook_waits (request_id, provider_session_id, agent_id, tool_name, input_hash, last_polled_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&request_id)
    .bind(sid)
    .bind(agent_id(input))
    .bind(&ask.tool_name)
    .bind(input_hash(&ask.tool_input))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or("");
    let mut kinds = Vec::new();
    if let Some(t) = crate::collect::title_event_if_needed(svc, sid, cwd).await? {
        kinds.push(t);
    }
    kinds.push(EventKind::DecisionRequested {
        request_id: request_id.clone(),
        title: None,
        questions: ask.questions.clone(),
        allow_free_text: ask.allow_free_text,
        rationale: ask.rationale.clone(),
        deadline_at: None,
    });
    for k in kinds {
        if let Err(e) = ingest(svc, sid, k).await {
            give_up(pool, &request_id, svc.now()).await?;
            return Err(e);
        }
    }

    let deadline = tokio::time::Instant::now() + wait;
    let mut errors = 0u32;
    let mut last_beat = tokio::time::Instant::now();
    loop {
        let step = poll_once(svc, &request_id, &mut last_beat).await;
        match step {
            Ok(Poll::Take(answers)) => return Ok(Some(hook_output(&ask, &answers))),
            Ok(Poll::Closed) => {
                // 맥에서 먼저 답했거나 세션이 끝남
                let _ = give_up(pool, &request_id, svc.now()).await;
                return Ok(None);
            }
            Ok(Poll::Wait) => errors = 0,
            Err(e) => {
                // 잠금 대기 초과 같은 일시 오류는 다시 시도한다
                errors += 1;
                if errors >= MAX_POLL_ERRORS {
                    let _ = give_up(pool, &request_id, svc.now()).await;
                    return Err(e);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    // 기다림 끝: 폰으로는 더 답할 수 없다. 맥 화면의 창은 그대로 있으므로 '입력 대기(맥에서 확인)'로 돌린다.
    // (철회가 실패해도 서버의 정리 작업이 '닫힌 우편함·열린 결정'을 철회한다)
    if give_up(pool, &request_id, svc.now()).await? {
        ingest(
            svc,
            sid,
            EventKind::DecisionWithdrawn {
                request_id: request_id.clone(),
            },
        )
        .await?;
        ingest(
            svc,
            sid,
            EventKind::PermissionRequested {
                detail: Some("권한".into()),
            },
        )
        .await?;
        Ok(None)
    } else if let Some(answers) = try_take(pool, &request_id, svc.now()).await? {
        // 닫기 직전에 답이 들어옴
        Ok(Some(hook_output(&ask, &answers)))
    } else {
        Ok(None)
    }
}

enum Poll {
    Take(Answers),
    Closed,
    Wait,
}

/// 우편함을 한 번 살핀다. 쓰기 잠금을 줄이려고 읽기로 먼저 보고, 답이 있을 때만 가져가기를 쓴다.
async fn poll_once(
    svc: &Service,
    request_id: &str,
    last_beat: &mut tokio::time::Instant,
) -> Result<Poll> {
    let pool = svc.pool();
    let row = sqlx::query(
        "SELECT w.answers IS NOT NULL AS has_answer, d.state FROM hook_waits w \
         LEFT JOIN sessions s ON s.provider = ? AND s.provider_session_id = w.provider_session_id \
         LEFT JOIN decisions d ON d.session_id = s.id AND d.provider_request_id = w.request_id \
         WHERE w.request_id = ?",
    )
    .bind(CLAUDE)
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Poll::Closed);
    };
    let has_answer: bool = row.try_get("has_answer")?;
    let state: Option<String> = row.try_get("state")?;
    if has_answer && let Some(a) = try_take(pool, request_id, svc.now()).await? {
        return Ok(Poll::Take(a));
    }
    if !matches!(state.as_deref(), Some("open" | "sending" | "confirming")) {
        return Ok(Poll::Closed);
    }
    if last_beat.elapsed() >= BEAT {
        sqlx::query("UPDATE hook_waits SET last_polled_at = ? WHERE request_id = ?")
            .bind(svc.now())
            .bind(request_id)
            .execute(pool)
            .await?;
        *last_beat = tokio::time::Instant::now();
    }
    Ok(Poll::Wait)
}

/// 같은 세션의 뒤따르는 훅으로 '맥에서 이미 답함'을 알아챈다(맥 답에는 따로 오는 신호가 없다).
/// - PostToolUse·PostToolUseFailure: 같은 도구·같은 입력의 기다림, 그리고 같은 에이전트에서
///   창이 생기고 3초 넘게 지난 기다림(맥에서 거부한 뒤 다른 도구로 넘어간 경우)
/// - Stop·StopFailure·UserPromptSubmit·SessionEnd: 그 세션의 모든 기다림(창이 이미 닫힘)
///
/// SessionEnd는 세션 종료 처리가 결정을 닫으므로 우편함만 닫는다. 닫은 건수를 돌려준다.
pub async fn close_answered_elsewhere(svc: &Service, input: &Value) -> Result<usize> {
    let Some(sid) = input.get("session_id").and_then(Value::as_str) else {
        return Ok(0);
    };
    let ev = input
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pool = svc.pool();
    let rows = match ev {
        "PostToolUse" | "PostToolUseFailure" => {
            let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let hash = input_hash(input.get("tool_input").unwrap_or(&Value::Null));
            sqlx::query(
                "SELECT request_id FROM hook_waits WHERE provider_session_id = ? AND agent_id = ? \
                 AND taken_at IS NULL AND gave_up_at IS NULL \
                 AND ((tool_name = ? AND input_hash = ?) OR created_at <= ?)",
            )
            .bind(sid)
            .bind(agent_id(input))
            .bind(tool)
            .bind(hash)
            .bind(svc.now() - PARALLEL_GRACE_MS)
            .fetch_all(pool)
            .await?
        }
        "Stop" | "StopFailure" | "UserPromptSubmit" | "SessionEnd" => {
            open_waits(pool, sid).await?
        }
        _ => return Ok(0),
    };
    let mut n = 0;
    for r in rows {
        let request_id: String = r.try_get("request_id")?;
        if !give_up(pool, &request_id, svc.now()).await? {
            continue; // 그 사이 폰 답을 가져감 = 이 앱의 답
        }
        n += 1;
        if ev != "SessionEnd" {
            ingest(svc, sid, EventKind::DecisionAnsweredElsewhere { request_id }).await?;
        }
    }
    Ok(n)
}

async fn open_waits(pool: &SqlitePool, sid: &str) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
    Ok(sqlx::query(
        "SELECT request_id FROM hook_waits WHERE provider_session_id = ? AND taken_at IS NULL AND gave_up_at IS NULL",
    )
    .bind(sid)
    .fetch_all(pool)
    .await?)
}

/// 서버 정리 작업(1분마다):
/// - 살아 있다는 표시가 `HOOK_DEAD_MS` 넘게 없는 기다림(훅이 죽음)을 닫고 결정을 철회한다.
/// - 우편함은 닫혔는데 결정이 아직 열린 것(닫는 도중 끊김)을 1분 뒤 철회한다.
/// - 끝난 우편함(답 내용 포함)은 하루 뒤 지운다.
pub async fn sweep_stale_waits(svc: &Service) -> Result<usize> {
    let pool = svc.pool();
    let now = svc.now();
    let dead = sqlx::query(
        "SELECT request_id, provider_session_id FROM hook_waits \
         WHERE taken_at IS NULL AND gave_up_at IS NULL AND COALESCE(last_polled_at, created_at) <= ?",
    )
    .bind(now - HOOK_DEAD_MS)
    .fetch_all(pool)
    .await?;
    let mut n = 0;
    for r in dead {
        let request_id: String = r.try_get("request_id")?;
        let sid: String = r.try_get("provider_session_id")?;
        if give_up(pool, &request_id, now).await? {
            ingest(svc, &sid, EventKind::DecisionWithdrawn { request_id }).await?;
            n += 1;
        }
    }
    let stuck = sqlx::query(
        "SELECT w.request_id, w.provider_session_id FROM hook_waits w \
         JOIN sessions s ON s.provider = ? AND s.provider_session_id = w.provider_session_id \
         JOIN decisions d ON d.session_id = s.id AND d.provider_request_id = w.request_id \
         WHERE w.taken_at IS NULL AND w.gave_up_at <= ? AND d.state IN ('open','sending','confirming')",
    )
    .bind(CLAUDE)
    .bind(now - 60_000)
    .fetch_all(pool)
    .await?;
    for r in stuck {
        let request_id: String = r.try_get("request_id")?;
        let sid: String = r.try_get("provider_session_id")?;
        ingest(svc, &sid, EventKind::DecisionWithdrawn { request_id }).await?;
        n += 1;
    }
    sqlx::query(
        "DELETE FROM hook_waits WHERE (taken_at IS NOT NULL OR gave_up_at IS NOT NULL) AND created_at <= ?",
    )
    .bind(now - 24 * 3600 * 1000)
    .execute(pool)
    .await?;
    Ok(n)
}

/// 서버 쪽 Claude Code 어댑터: 폰 답을 우편함에 넣고 훅이 가져갔는지 확인한다.
pub struct ClaudeHookProvider {
    pool: SqlitePool,
    clock: Arc<dyn Clock>,
    take_wait: Duration,
}

impl ClaudeHookProvider {
    pub fn new(pool: SqlitePool, clock: Arc<dyn Clock>) -> Self {
        Self {
            pool,
            clock,
            take_wait: TAKE_WAIT,
        }
    }

    pub fn with_take_wait(mut self, d: Duration) -> Self {
        self.take_wait = d;
        self
    }

    /// (가져감, 닫힘) — 행이 없으면 None. 훅이 사라졌으면(살아 있다는 표시가 끊김) 여기서 닫는다.
    async fn mailbox(&self, request_id: &str) -> Result<Option<(bool, bool)>> {
        let now = self.clock.now_ms();
        sqlx::query(
            "UPDATE hook_waits SET gave_up_at = ? WHERE request_id = ? AND taken_at IS NULL \
             AND gave_up_at IS NULL AND COALESCE(last_polled_at, created_at) <= ?",
        )
        .bind(now)
        .bind(request_id)
        .bind(now - HOOK_DEAD_MS)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query("SELECT taken_at, gave_up_at FROM hook_waits WHERE request_id = ?")
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => Some((
                r.try_get::<Option<i64>, _>("taken_at")?.is_some(),
                r.try_get::<Option<i64>, _>("gave_up_at")?.is_some(),
            )),
            None => None,
        })
    }

    async fn deliver(&self, target: &DeliveryTarget, answers: &Answers) -> Result<DeliveryOutcome> {
        let written = sqlx::query(
            "UPDATE hook_waits SET answers = ?, answered_at = ? \
             WHERE request_id = ? AND provider_session_id = ? \
               AND answers IS NULL AND taken_at IS NULL AND gave_up_at IS NULL",
        )
        .bind(serde_json::to_string(answers)?)
        .bind(self.clock.now_ms())
        .bind(&target.provider_request_id)
        .bind(&target.provider_session_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        if !written {
            return Ok(match self.mailbox(&target.provider_request_id).await? {
                None => DeliveryOutcome::NotApplied("no_hook".into()),
                Some((true, _)) => DeliveryOutcome::Accepted,
                Some((false, true)) => DeliveryOutcome::NotApplied("hook_gone".into()),
                Some((false, false)) => DeliveryOutcome::Unknown("already_sent".into()),
            });
        }
        let deadline = tokio::time::Instant::now() + self.take_wait;
        loop {
            match self.mailbox(&target.provider_request_id).await? {
                Some((true, _)) => return Ok(DeliveryOutcome::Accepted),
                // 훅이 가져가지 않고 끝남(맥에서 먼저 답함·시간 끝) = 확실히 반영되지 않음
                Some((false, true)) | None => {
                    return Ok(DeliveryOutcome::NotApplied("hook_gone".into()));
                }
                Some((false, false)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(DeliveryOutcome::Unknown("hook_slow".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl ProviderClient for ClaudeHookProvider {
    fn deliver_answer<'a>(
        &'a self,
        target: &'a DeliveryTarget,
        answers: &'a Answers,
    ) -> BoxFuture<'a, DeliveryOutcome> {
        Box::pin(async move {
            self.deliver(target, answers)
                .await
                .unwrap_or_else(|e| DeliveryOutcome::Unknown(format!("db: {e}")))
        })
    }

    fn request_status<'a>(&'a self, target: &'a DeliveryTarget) -> BoxFuture<'a, RequestStatus> {
        Box::pin(async move {
            match self.mailbox(&target.provider_request_id).await {
                Ok(Some((true, _))) => RequestStatus::Answered,
                Ok(Some((false, true))) | Ok(None) => RequestStatus::Gone,
                _ => RequestStatus::Unknown,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm_input(tool: &str, tool_input: Value) -> Value {
        json!({"session_id": "s1", "hook_event_name": "PermissionRequest", "cwd": "/w/proj",
               "tool_name": tool, "tool_input": tool_input})
    }

    #[test]
    fn bash_permission_becomes_allow_deny_question() {
        let ask = permission_ask(&perm_input(
            "Bash",
            json!({"command": "rm -rf build", "description": "빌드 폴더 지우기"}),
        ))
        .unwrap();
        assert_eq!(ask.questions.len(), 1);
        assert_eq!(ask.questions[0].options[0].label, ALLOW);
        assert!(!ask.allow_free_text);
        assert_eq!(
            ask.rationale.as_deref(),
            Some("빌드 폴더 지우기\n\n$ rm -rf build")
        );

        let allow = hook_output(
            &ask,
            &Answers::from([(ask.questions[0].question.clone(), AnswerValue::One(ALLOW.into()))]),
        );
        assert_eq!(
            allow["hookSpecificOutput"]["decision"],
            json!({"behavior": "allow"})
        );
        let deny = hook_output(
            &ask,
            &Answers::from([(ask.questions[0].question.clone(), AnswerValue::One(DENY.into()))]),
        );
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(deny["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
    }

    #[test]
    fn question_answers_go_into_updated_input() {
        let qs = json!([{"question": "색은?", "header": "색", "multiSelect": false,
                         "options": [{"label": "빨강"}, {"label": "파랑"}]},
                        {"question": "과일은?", "multiSelect": true,
                         "options": [{"label": "사과"}, {"label": "배"}]}]);
        let ask = permission_ask(&perm_input(ASK_TOOL, json!({"questions": qs}))).unwrap();
        assert!(ask.allow_free_text);
        assert_eq!(ask.questions.len(), 2);
        let out = hook_output(
            &ask,
            &Answers::from([
                ("색은?".into(), AnswerValue::One("파랑".into())),
                (
                    "과일은?".into(),
                    AnswerValue::Many(vec!["사과".into(), "배".into()]),
                ),
            ]),
        );
        let d = &out["hookSpecificOutput"]["decision"];
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"]["questions"], qs);
        assert_eq!(
            d["updatedInput"]["answers"],
            json!({"색은?": "파랑", "과일은?": "사과, 배"})
        );
    }

    #[test]
    fn hash_ignores_answers_and_key_order() {
        let a = json!({"questions": [{"question": "q"}]});
        let b = json!({"questions": [{"question": "q"}], "answers": {"q": "x"}});
        assert_eq!(input_hash(&a), input_hash(&b));
        let c: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let d: Value = serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap();
        assert_eq!(input_hash(&c), input_hash(&d));
        assert_ne!(input_hash(&a), input_hash(&c));
    }

    #[test]
    fn bad_question_input_is_not_answerable() {
        assert!(permission_ask(&perm_input(ASK_TOOL, json!({"questions": []}))).is_none());
        assert!(permission_ask(&perm_input(ASK_TOOL, json!({}))).is_none());
        assert!(permission_ask(&json!({"session_id": "s"})).is_none());
    }
}
