//! 2c단계 시험: PermissionRequest 훅이 폰 답을 기다렸다가 Claude Code에 돌려주는 흐름.
//! 훅 프로세스와 서버(제공자 어댑터)가 같은 DB 우편함으로 만나는 것을 한 프로세스에서 재현한다.

mod common;

use ai_task_notifier::claude_answer::{self, ALLOW, ClaudeHookProvider};
use ai_task_notifier::collect;
use ai_task_notifier::model::{AnswerValue, Answers, CloseReason, DecisionState, SessionState};
use ai_task_notifier::service::{DecisionDetail, Service, SubmitRequest};
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const SID: &str = "sess-perm-0001";

fn input(ev: &str, extra: Value) -> Value {
    let mut v = json!({"session_id": SID, "cwd": "/Users/x/work/my-app", "hook_event_name": ev});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

fn bash() -> Value {
    json!({"tool_name": "Bash", "tool_input": {"command": "cargo publish", "description": "배포"}})
}

fn ask() -> Value {
    json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [
        {"question": "색은?", "header": "색", "multiSelect": false,
         "options": [{"label": "빨강"}, {"label": "파랑"}]}]}})
}

async fn h() -> H {
    harness_with(|s: Service| {
        let p = ClaudeHookProvider::new(s.pool().clone(), s.clock())
            .with_take_wait(Duration::from_secs(3));
        s.with_provider(CLAUDE, Arc::new(p))
    })
    .await
}

/// 훅을 띄우고 결정이 생길 때까지 기다린다. (훅 작업, 결정 상세)
async fn start_hook(
    h: &H,
    tool: Value,
    wait: Duration,
) -> (tokio::task::JoinHandle<Option<Value>>, DecisionDetail) {
    let svc = h.svc.clone();
    let v = input("PermissionRequest", tool);
    let task = tokio::spawn(async move {
        claude_answer::run_permission_hook(&svc, &v, wait)
            .await
            .unwrap()
    });
    for _ in 0..100 {
        let snap = h.svc.snapshot().await.unwrap();
        if let Some(d) = snap.decisions.first() {
            let detail = h.svc.decision_detail(&d.id).await.unwrap();
            return (task, detail);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("결정이 생기지 않았습니다");
}

async fn hook(h: &H, ev: &str, extra: Value) {
    collect::run_claude_hook(&h.svc, &input(ev, extra).to_string())
        .await
        .unwrap();
}

async fn state(h: &H) -> SessionState {
    h.svc
        .session_brief(CLAUDE, SID)
        .await
        .unwrap()
        .unwrap()
        .state
}

fn answer(d: &DecisionDetail, label: &str) -> SubmitRequest {
    SubmitRequest {
        submission_id: format!("sub-{}", d.summary.id),
        decision_version: d.summary.version,
        answers: Answers::from([(
            d.questions[0].question.clone(),
            AnswerValue::One(label.into()),
        )]),
    }
}

#[tokio::test]
async fn phone_answer_reaches_waiting_hook() {
    let h = h().await;
    let (task, d) = start_hook(&h, bash(), Duration::from_secs(10)).await;
    assert_eq!(state(&h).await, SessionState::WaitingDecision);
    assert_eq!(d.rationale.as_deref(), Some("배포\n\n$ cargo publish"));
    assert_eq!(d.summary.title, "my-app");
    // 같은 창의 맥 알림(약 6초 뒤)이 와도 '결정 대기'를 유지한다
    hook(&h, "Notification", json!({"notification_type": "permission_prompt"})).await;
    assert_eq!(state(&h).await, SessionState::WaitingDecision);

    h.svc.submit(None, &d.summary.id, &answer(&d, ALLOW)).await.unwrap();
    assert_eq!(h.svc.deliver_pending().await.unwrap(), 1);
    let out = task.await.unwrap().expect("훅이 답을 출력해야 합니다");
    assert_eq!(
        out["hookSpecificOutput"],
        json!({"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}})
    );
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.summary.state, DecisionState::Applied);
    assert_eq!(after.closed_reason, Some(CloseReason::ThisApp));
    assert_eq!(state(&h).await, SessionState::Running);

    // 이어서 그 도구가 실행돼도 '다른 곳에서 답함'으로 바뀌지 않는다
    hook(&h, "PostToolUse", bash()).await;
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::ThisApp));
}

#[tokio::test]
async fn mac_answer_first_releases_hook() {
    let h = h().await;
    let (task, d) = start_hook(&h, ask(), Duration::from_secs(10)).await;
    assert!(d.allow_free_text);
    // 맥에서 답함 → 답이 붙은 입력으로 PostToolUse가 온다
    let mut done = ask();
    done["tool_input"]["answers"] = json!({"색은?": "빨강"});
    hook(&h, "PostToolUse", done).await;

    let out = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("훅이 곧 끝나야 합니다")
        .unwrap();
    assert_eq!(out, None);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::Elsewhere));
    // 늦게 보낸 폰 답은 받지 않는다
    assert!(h.svc.submit(None, &d.summary.id, &answer(&d, "파랑")).await.is_err());
}

#[tokio::test]
async fn other_tool_finishing_does_not_close_wait() {
    let h = h().await;
    let (task, d) = start_hook(&h, bash(), Duration::from_secs(10)).await;
    // 다른 도구(예: 하위 에이전트)가 끝난 것은 이 창과 무관
    hook(
        &h,
        "PostToolUse",
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
    )
    .await;
    let mid = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(mid.summary.state, DecisionState::Open);
    // 턴이 끝나면 창도 닫힌 것 → 다른 곳에서 답함
    hook(&h, "Stop", json!({})).await;
    assert_eq!(task.await.unwrap(), None);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::Elsewhere));
}

#[tokio::test]
async fn hook_gives_up_after_wait() {
    let h = h().await;
    let (task, d) = start_hook(&h, bash(), Duration::from_millis(600)).await;
    assert_eq!(task.await.unwrap(), None);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::Withdrawn));
    // 맥 화면의 창은 그대로이므로 '입력 대기(맥에서 확인)'
    assert_eq!(state(&h).await, SessionState::WaitingPermission);
}

#[tokio::test]
async fn mac_denial_then_other_tool_closes_wait() {
    // 맥에서 거부하면 그 도구의 PostToolUse는 없다. 같은 에이전트의 다른 도구가 3초 넘어 끝나면 닫는다.
    let h = h().await;
    let (task, d) = start_hook(&h, bash(), Duration::from_secs(10)).await;
    // 하위 에이전트의 도구는 주 대화의 창과 무관
    h.clock.advance(5_000);
    hook(
        &h,
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {"file_path": "a"}, "agent_id": "sub-1"}),
    )
    .await;
    let mid = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(mid.summary.state, DecisionState::Open);
    hook(
        &h,
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {"file_path": "a"}}),
    )
    .await;
    assert_eq!(task.await.unwrap(), None);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::Elsewhere));
}

#[tokio::test]
async fn hook_gone_means_not_applied() {
    // 훅이 가져가지 않은 채 사라지면(강제 종료) 어댑터는 '반영 안 됨'을 확실히 알 수 있다
    let h = h().await;
    let (task, d) = start_hook(&h, ask(), Duration::from_secs(10)).await;
    task.abort();
    // 살아 있다는 표시가 15초 넘게 없음
    h.clock.advance(20_000);
    h.svc.submit(None, &d.summary.id, &answer(&d, "파랑")).await.unwrap();
    h.svc.deliver_pending().await.unwrap();
    let mid = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(
        mid.latest_submission.map(|s| (s.state, s.detail)),
        Some((
            ai_task_notifier::model::SubmissionState::Rejected,
            Some("hook_gone".into())
        ))
    );
    // 정리 작업: 닫힌 우편함인데 결정이 열려 있으면 1분 뒤 철회
    h.clock.advance(61_000);
    assert_eq!(claude_answer::sweep_stale_waits(&h.svc).await.unwrap(), 1);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.summary.state, DecisionState::Closed);
    assert_eq!(after.closed_reason, Some(CloseReason::Withdrawn));
}

#[tokio::test]
async fn dead_hook_is_swept() {
    let h = h().await;
    let (task, d) = start_hook(&h, bash(), Duration::from_secs(10)).await;
    task.abort();
    assert_eq!(claude_answer::sweep_stale_waits(&h.svc).await.unwrap(), 0);
    h.clock.advance(16_000);
    assert_eq!(claude_answer::sweep_stale_waits(&h.svc).await.unwrap(), 1);
    let after = h.svc.decision_detail(&d.summary.id).await.unwrap();
    assert_eq!(after.closed_reason, Some(CloseReason::Withdrawn));
}
