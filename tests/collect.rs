//! 실제 데이터 수집 시험: Claude 훅과 Codex 기록 감시를 DB까지 거쳐 확인한다.

mod common;

use ai_task_notifier::collect::{self, CodexWatcher};
use ai_task_notifier::model::SessionState;
use common::*;
use std::io::Write;

fn hook(ev: &str, extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({"session_id": "sess-aaaa-bbbb-cccc", "cwd": "/Users/x/work/my-app", "hook_event_name": ev});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    v.to_string()
}

#[tokio::test]
async fn claude_hooks_drive_session_state() {
    let h = harness().await;
    let svc = &h.svc;
    let sid = "sess-aaaa-bbbb-cccc";
    // 훅을 켜기 전에 시작한 세션: 처음 본 것이 도구 완료여도 폴더 이름 제목으로 진행 중 등록
    collect::run_claude_hook(
        svc,
        &hook("PostToolUse", serde_json::json!({"tool_name": "Bash"})),
    )
    .await
    .unwrap();
    let b = svc
        .session_brief(collect::CLAUDE, sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (b.state, b.title.as_str()),
        (SessionState::Running, "my-app")
    );
    // 이미 진행 중이면 도구 완료는 쓰지 않는다(10분 안)
    let v = svc.snapshot_version().await.unwrap();
    collect::run_claude_hook(
        svc,
        &hook("PostToolUse", serde_json::json!({"tool_name": "Read"})),
    )
    .await
    .unwrap();
    assert_eq!(svc.snapshot_version().await.unwrap(), v);
    // 질문 → 입력 대기, 도구 완료 → 해제
    collect::run_claude_hook(
        svc,
        &hook(
            "PreToolUse",
            serde_json::json!({"tool_name": "AskUserQuestion"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(h.counts().await.0, 0);
    collect::run_claude_hook(
        svc,
        &hook(
            "PostToolUse",
            serde_json::json!({"tool_name": "AskUserQuestion"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(h.counts().await.0, 1);
    // Esc로 멈춤(Stop 없음) → idle_prompt 알림으로 유휴
    collect::run_claude_hook(
        svc,
        &hook(
            "Notification",
            serde_json::json!({"notification_type": "idle_prompt"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(h.counts().await.0, 0);
    // 종료 후 재개(SessionStart)하면 되살아남
    collect::run_claude_hook(svc, &hook("SessionEnd", serde_json::json!({})))
        .await
        .unwrap();
    assert!(h.svc.snapshot().await.unwrap().sessions.is_empty());
    collect::run_claude_hook(
        svc,
        &hook("SessionStart", serde_json::json!({"source": "resume"})),
    )
    .await
    .unwrap();
    assert_eq!(h.svc.snapshot().await.unwrap().sessions.len(), 1);
    // 잘못된 입력은 오류(호출한 쪽이 삼킴), 세션 ID 없는 입력은 무시
    assert!(collect::run_claude_hook(svc, "not json").await.is_err());
    collect::run_claude_hook(svc, r#"{"hook_event_name":"Stop"}"#)
        .await
        .unwrap();
}

#[tokio::test]
async fn placeholder_title_is_replaced_by_folder_name() {
    let h = harness().await;
    let sid = "sess-aaaa-bbbb-cccc";
    // 제목 없이 먼저 생긴 세션(제목 = ID 앞 12자)
    h.send(ev(
        collect::CLAUDE,
        sid,
        ai_task_notifier::model::EventKind::TurnEnded,
    ))
    .await;
    assert_eq!(
        h.svc
            .session_brief(collect::CLAUDE, sid)
            .await
            .unwrap()
            .unwrap()
            .title,
        "sess-aaaa-bb"
    );
    collect::run_claude_hook(&h.svc, &hook("UserPromptSubmit", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        h.svc
            .session_brief(collect::CLAUDE, sid)
            .await
            .unwrap()
            .unwrap()
            .title,
        "my-app"
    );
}

fn line(v: serde_json::Value) -> String {
    format!("{v}\n")
}

#[tokio::test]
async fn codex_watcher_reads_new_lines_once() {
    let h = harness().await;
    let home = tempfile::tempdir().unwrap();
    let day = home.path().join("sessions/2026/11/26");
    std::fs::create_dir_all(&day).unwrap();
    let f = day.join("rollout-2026-11-26T10-00-00-abc.jsonl");
    let mut file = std::fs::File::create(&f).unwrap();
    write!(file, "{}", line(serde_json::json!({"type": "session_meta", "payload": {"id": "cx-1", "cwd": "/w/api-tests"}}))).unwrap();
    write!(
        file,
        "{}",
        line(serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}))
    )
    .unwrap();
    // 깨진 글자·대화 내용 줄은 멈추지 않고 건너뛴다
    file.write_all(b"{\"type\":\"response_item\",\"payload\":{\"text\":\"\xff\xfe\"}}\n")
        .unwrap();
    write!(file, "{}", line(serde_json::json!({"timestamp": "2026-11-26T01:00:00Z", "type": "event_msg", "payload": {"type": "token_count",
        "rate_limits": {"primary": {"used_percent": 35.0, "window_minutes": 300, "resets_at": 1_900_000_000},
                        "secondary": {"used_percent": 20.0, "window_minutes": 10080, "resets_at": 1_900_100_000}}}}))).unwrap();
    drop(file);

    let mut w = CodexWatcher::new(home.path());
    w.poll(&h.svc).await.unwrap();
    let snap = h.svc.snapshot().await.unwrap();
    assert_eq!(snap.running, 1);
    assert_eq!(snap.sessions[0].title, "api-tests");
    let u: Vec<_> = snap
        .usage
        .iter()
        .map(|u| (u.window.as_str(), u.used_percent))
        .collect();
    assert_eq!(u, vec![("five_hour", 35.0), ("seven_day", 20.0)]);
    assert_eq!(
        snap.usage[0].observed_at.to_rfc3339(),
        "2026-11-26T01:00:00+00:00",
        "관측 시각은 기록 시각"
    );

    // 이어 쓴 줄만 반영(쓰는 중인 마지막 줄은 다음에)
    let mut file = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
    write!(file, "{}", line(serde_json::json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": "비밀"}}))).unwrap();
    write!(file, "{{\"type\":\"event_msg\"").unwrap();
    drop(file);
    w.poll(&h.svc).await.unwrap();
    assert_eq!(h.counts().await.0, 0, "작업 완료 → 유휴");

    // 서버를 다시 켜도(새 감시자) 같은 줄을 두 번 반영하지 않는다
    let v = h.svc.snapshot_version().await.unwrap();
    let mut w2 = CodexWatcher::new(home.path());
    w2.poll(&h.svc).await.unwrap();
    assert_eq!(h.svc.snapshot_version().await.unwrap(), v);
    let dump: String = sqlx::query_scalar("SELECT group_concat(title) FROM sessions")
        .fetch_one(h.svc.pool())
        .await
        .unwrap();
    assert!(!dump.contains("비밀"), "대화 내용은 저장하지 않음");
}
