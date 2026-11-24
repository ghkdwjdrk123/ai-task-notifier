//! 설계 3장 인수 테스트 중 1단계(서버 로직) 범위: T01~T09, T16~T20과 관련 보조 시험.
//! 시험 이름 앞의 tNN은 설계 문서의 테스트 ID다.

mod common;

use ai_task_notifier::model::{
    AnswerValue, Answers, CloseReason, DecisionState, DeliveryOutcome, EventKind, RequestStatus,
    SessionOutcome, SessionState, SubmissionState,
};
use ai_task_notifier::push::PushOutcome;
use ai_task_notifier::service::{ServiceError, SubmitRequest};
use common::*;

const QD: &str = "중복 발견 시 어떻게 할까요?";

fn answers(pairs: &[(&str, &str)]) -> Answers {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), AnswerValue::One((*v).to_string())))
        .collect()
}

fn submit_req(id: &str, version: i64, a: Answers) -> SubmitRequest {
    SubmitRequest {
        submission_id: id.into(),
        decision_version: version,
        answers: a,
    }
}

async fn version_of(h: &H, id: &str) -> i64 {
    h.svc.decision_detail(id).await.unwrap().summary.version
}

fn conflict_code(e: ServiceError) -> &'static str {
    match e {
        ServiceError::Conflict { code, .. } => code,
        other => panic!("Conflict가 아님: {other:?}"),
    }
}

// ---------- F01 기기 ----------

#[tokio::test]
async fn t01_pairing_code_is_single_use() {
    let h = harness().await;
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let (dev, token) = h.svc.exchange_pairing_code(&code, "갤럭시").await.unwrap();
    assert_eq!(dev.name, "갤럭시");
    assert_eq!(
        h.svc.authenticate(&token).await.unwrap().unwrap().id,
        dev.id
    );
    // 같은 코드 재제출은 거절(소문자·공백도 같은 코드로 본다)
    let again = h
        .svc
        .exchange_pairing_code(&code.to_lowercase(), "다른 폰")
        .await
        .unwrap_err();
    assert_eq!(conflict_code(again), "invalid_code");
    // 만료된 코드도 거절
    let code2 = h.svc.create_pairing_code(600_000).await.unwrap();
    h.clock.advance(600_000);
    assert_eq!(
        conflict_code(h.svc.exchange_pairing_code(&code2, "폰").await.unwrap_err()),
        "invalid_code"
    );
    assert_eq!(h.svc.list_devices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn t02_revoked_device_loses_access_and_pending_push() {
    let h = harness().await;
    let (dev, token) = h.device_with_push("갤럭시").await;
    h.start(CLAUDE, "s1").await;
    h.decide(
        "s1",
        "r1",
        vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
        None,
    )
    .await;
    assert_eq!(h.outbox_states().await, vec!["pending"]);

    assert!(h.svc.revoke_device(&dev).await.unwrap());
    assert!(
        h.svc.authenticate(&token).await.unwrap().is_none(),
        "해제 후 인증 거절"
    );
    let stats = h.svc.process_push_outbox(&h.push, 10).await.unwrap();
    assert_eq!(stats.sent, 0);
    assert!(
        h.push.sent().is_empty(),
        "해제한 기기로 푸시가 나가면 안 됨"
    );
    assert_eq!(h.outbox_states().await, vec!["cancelled"]);
    let devices = h.svc.list_devices().await.unwrap();
    assert!(devices[0].revoked && !devices[0].has_push);
}

// ---------- F02·F03 세션 수 ----------

#[tokio::test]
async fn t03_top_level_sessions_counted_once() {
    let h = harness().await;
    h.start(CLAUDE, "c1").await;
    h.start(CLAUDE, "c2").await;
    h.start(CODEX, "x1").await;
    for _ in 0..3 {
        h.send(ev(CLAUDE, "c1", EventKind::SubagentActivity)).await;
    }
    // 같은 세션의 턴이 다시 시작돼도 한 건
    h.start(CLAUDE, "c1").await;
    assert_eq!(h.counts().await, (3, 0));
}

#[tokio::test]
async fn t04_completed_interrupted_failed_reduce_running() {
    let h = harness().await;
    let _dev = h.device_with_push("폰").await;
    for s in ["a", "b", "c"] {
        h.start(CLAUDE, s).await;
    }
    let mut seen = vec![h.counts().await.0];
    for (s, o) in [
        ("a", SessionOutcome::Completed),
        ("b", SessionOutcome::Interrupted),
        ("c", SessionOutcome::Failed),
    ] {
        h.send(ev(CLAUDE, s, EventKind::SessionEnded { outcome: o }))
            .await;
        seen.push(h.counts().await.0);
    }
    assert_eq!(seen, vec![3, 2, 1, 0]);
    assert!(h.outbox_states().await.is_empty(), "결정 푸시 없음");
}

#[tokio::test]
async fn t05_turn_end_with_question_mark_is_not_a_decision() {
    let h = harness().await;
    let _dev = h.device_with_push("폰").await;
    h.start(CLAUDE, "s1").await;
    // 일반 답변에 물음표가 있어도 이벤트는 턴 종료일 뿐이다
    h.send(ev(CLAUDE, "s1", EventKind::TurnEnded)).await;
    assert_eq!(h.counts().await, (0, 0));
    assert!(h.outbox_states().await.is_empty());
    let snap = h.svc.snapshot().await.unwrap();
    assert_eq!(snap.sessions[0].state, SessionState::Idle);
}

#[tokio::test]
async fn t06_permission_wait_is_not_a_project_decision() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    h.start(CLAUDE, "s2").await;
    h.send(ev(
        CLAUDE,
        "s2",
        EventKind::PermissionRequested {
            detail: Some("Bash".into()),
        },
    ))
    .await;
    assert_eq!(h.counts().await, (1, 0));
    let snap = h.svc.snapshot().await.unwrap();
    let s2 = snap
        .sessions
        .iter()
        .find(|s| s.state == SessionState::WaitingPermission);
    assert!(s2.is_some(), "세션 상세에 권한 승인 대기");
    h.send(ev(CLAUDE, "s2", EventKind::PermissionResolved))
        .await;
    assert_eq!(h.counts().await, (2, 0));
}

#[tokio::test]
async fn t07_count_changes_do_not_create_decision_pushes() {
    let h = harness().await;
    let _dev = h.device_with_push("폰").await;
    let v0 = h.svc.snapshot().await.unwrap().version;
    h.start(CLAUDE, "a").await;
    h.start(CLAUDE, "b").await;
    h.start(CLAUDE, "c").await;
    h.send(ev(CLAUDE, "a", EventKind::TurnEnded)).await;
    h.send(ev(CLAUDE, "b", EventKind::TurnEnded)).await;
    let snap = h.svc.snapshot().await.unwrap();
    assert_eq!((snap.running, snap.waiting_decisions), (1, 0));
    assert!(snap.version > v0, "숫자가 바뀌면 스냅샷 버전이 오른다");
    assert!(
        h.outbox_states().await.is_empty(),
        "숫자 변화로 배너·진동을 만들지 않음"
    );
}

// ---------- F04·F05 결정 ----------

#[tokio::test]
async fn t08_decision_request_moves_session_and_queues_one_push() {
    let h = harness().await;
    let (_d1, _) = h.device_with_push("폰1").await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    assert_eq!(h.counts().await, (0, 1));
    let stats = h.svc.process_push_outbox(&h.push, 10).await.unwrap();
    assert_eq!(stats.sent, 1);
    let sent = h.push.sent();
    assert_eq!(sent[0].payload["type"], "decision");
    assert_eq!(sent[0].payload["decision_id"], id.as_str());
    // T30: 푸시 본문은 정확히 유형·불투명 ID·버전뿐이다(질문·작업명 없음)
    let keys: Vec<&String> = sent[0].payload.as_object().unwrap().keys().collect();
    let mut keys: Vec<&str> = keys.into_iter().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["decision_id", "type", "v"]);
    // 다시 처리해도 또 보내지 않는다
    assert_eq!(
        h.svc.process_push_outbox(&h.push, 10).await.unwrap().sent,
        0
    );
    assert_eq!(h.svc.snapshot().await.unwrap().decisions.len(), 1);
}

#[tokio::test]
async fn t09_redelivered_request_is_deduplicated() {
    let h = harness().await;
    let _dev = h.device_with_push("폰").await;
    h.start(CLAUDE, "s1").await;
    let e = decision_event("s1", "r1", vec![q(QD, &["가", "나"])], None);
    let first = h.svc.ingest(&e).await.unwrap();
    // 같은 이벤트 재전달(같은 멱등 키)
    let dup = h.svc.ingest(&e).await.unwrap();
    assert!(dup.duplicate && !dup.changed);
    // 멱등 키는 다르지만 같은 원본 요청 ID
    let again = h
        .svc
        .ingest(&decision_event(
            "s1",
            "r1",
            vec![q(QD, &["가", "나"])],
            None,
        ))
        .await
        .unwrap();
    assert!(again.new_decision.is_none());
    let snap = h.svc.snapshot().await.unwrap();
    assert_eq!(snap.waiting_decisions, 1);
    assert_eq!(snap.decisions[0].id, first.new_decision.unwrap());
    assert_eq!(h.outbox_states().await.len(), 1, "배너도 한 건");
}

#[tokio::test]
async fn t10_two_requests_in_one_session_count_twice() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let a = h
        .decide(
            "s1",
            "A",
            vec![q("질문1", &["예", "아니요"]), q("질문2", &["예", "아니요"])],
            None,
        )
        .await;
    let b = h
        .decide("s1", "B", vec![q("질문3", &["예", "아니요"])], None)
        .await;
    assert_eq!(h.counts().await.1, 2);
    assert_eq!(h.svc.decision_detail(&a).await.unwrap().questions.len(), 2);
    assert_eq!(h.svc.decision_detail(&b).await.unwrap().questions.len(), 1);
}

#[tokio::test]
async fn t12_list_ordered_by_deadline_then_created() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let ten = h
        .decide(
            "s1",
            "ten",
            vec![q("10분", &["a", "b"])],
            Some(T0 + 600_000),
        )
        .await;
    h.clock.advance(1);
    let none = h
        .decide("s1", "none", vec![q("마감 없음", &["a", "b"])], None)
        .await;
    h.clock.advance(1);
    let five = h
        .decide(
            "s1",
            "five",
            vec![q("5분", &["a", "b"])],
            Some(T0 + 300_000),
        )
        .await;
    let ids: Vec<_> = h
        .svc
        .snapshot()
        .await
        .unwrap()
        .decisions
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, vec![five, ten, none]);
}

#[tokio::test]
async fn invalid_decision_is_rejected_without_side_effects() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    // 선택지 없는 질문인데 자유 입력도 불가
    let bad = decision_event("s1", "r1", vec![q("질문", &[])], None);
    assert!(matches!(
        h.svc.ingest(&bad).await,
        Err(ServiceError::Invalid(_))
    ));
    // 실패한 이벤트는 멱등 키도 남기지 않아 고쳐서 다시 보낼 수 있다
    assert!(!h.svc.ingest(&bad).await.is_ok_and(|r| r.duplicate));
    assert_eq!(h.counts().await, (1, 0));
}

// ---------- F07·F08 답변 ----------

#[tokio::test]
async fn t13_answer_goes_only_to_its_own_session() {
    let h = harness().await;
    h.start(CLAUDE, "A").await;
    h.start(CLAUDE, "B").await;
    let a = h
        .decide(
            "A",
            "ra",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    let b = h
        .decide(
            "B",
            "rb",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    let v = version_of(&h, &a).await;
    h.svc
        .submit(
            None,
            &a,
            &submit_req("sub-a-0001", v, answers(&[(QD, "중복 행만 건너뛰기")])),
        )
        .await
        .unwrap();
    assert_eq!(h.counts().await.1, 2, "반영 전에는 두 건");
    h.svc.deliver_pending().await.unwrap();
    let delivered = h.provider.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0.provider_session_id, "A");
    assert_eq!(delivered[0].0.provider_request_id, "ra");
    assert_eq!(h.counts().await, (1, 1), "A는 작업 재개, B는 계속 대기");
    assert_eq!(
        h.svc.decision_detail(&b).await.unwrap().summary.state,
        DecisionState::Open
    );
}

#[tokio::test]
async fn t16_same_submission_id_returns_existing_result() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    let v = version_of(&h, &id).await;
    let req = submit_req("sub-0000001", v, answers(&[(QD, "전체 중단")]));
    let (first, created) = h.svc.submit(None, &id, &req).await.unwrap();
    assert!(created);
    h.svc.deliver_pending().await.unwrap();
    // HTTP 응답이 유실돼 같은 제출 키로 다시 요청
    let (again, created2) = h.svc.submit(None, &id, &req).await.unwrap();
    assert!(!created2);
    assert_eq!(again.id, first.id);
    assert_eq!(again.state, SubmissionState::Applied);
    h.svc.deliver_pending().await.unwrap();
    assert_eq!(h.provider.delivered().len(), 1, "원본에 다시 보내지 않음");
    // 같은 ID에 다른 입력은 충돌
    let other = submit_req("sub-0000001", v, answers(&[(QD, "중복 행만 건너뛰기")]));
    assert_eq!(
        conflict_code(h.svc.submit(None, &id, &other).await.unwrap_err()),
        "submission_id_reused"
    );
}

#[tokio::test]
async fn t17_unknown_outcome_is_not_resent_and_waits_for_confirmation() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    let v = version_of(&h, &id).await;
    h.provider
        .push_outcome(DeliveryOutcome::Unknown("연결 끊김".into()));
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-0000017", v, answers(&[(QD, "전체 중단")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Confirming);
    assert!(!d.answerable);
    assert_eq!(h.counts().await.1, 1, "확인 전에는 대기 수 유지");
    // 자동 재전송 없음
    h.svc.deliver_pending().await.unwrap();
    assert_eq!(h.provider.delivered().len(), 1);
    // 원본 상태를 아직 모름 → 그대로
    h.svc.reconcile_confirming().await.unwrap();
    assert_eq!(
        h.svc.decision_detail(&id).await.unwrap().summary.state,
        DecisionState::Confirming
    );
    // 원본에서 반영이 확인되면 완료
    h.provider.set_status("r1", RequestStatus::Answered);
    assert_eq!(h.svc.reconcile_confirming().await.unwrap(), 1);
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Applied);
    assert_eq!(d.latest_submission.unwrap().state, SubmissionState::Applied);
    assert_eq!(h.counts().await, (1, 0));
}

#[tokio::test]
async fn t18_not_applied_returns_to_open_and_allows_retry() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            None,
        )
        .await;
    let v1 = version_of(&h, &id).await;
    h.provider
        .push_outcome(DeliveryOutcome::NotApplied("형식 오류".into()));
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-0000018", v1, answers(&[(QD, "전체 중단")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Open);
    assert!(d.answerable);
    let first = d.latest_submission.unwrap();
    assert_eq!(first.state, SubmissionState::Rejected);
    // 옛 버전으로는 다시 못 보냄
    let stale = submit_req("sub-0000019", v1, answers(&[(QD, "전체 중단")]));
    assert_eq!(
        conflict_code(h.svc.submit(None, &id, &stale).await.unwrap_err()),
        "stale_version"
    );
    // 새 버전·새 제출 ID로 재시도
    let v2 = d.summary.version;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-0000020", v2, answers(&[(QD, "전체 중단")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Applied);
    assert_eq!(
        h.svc.get_submission(&id, &first.id).await.unwrap().state,
        SubmissionState::Rejected,
        "처음 실패는 실패로 남는다"
    );
}

#[tokio::test]
async fn t19_deadline_boundary_rejects_and_clears() {
    let h = harness().await;
    let _dev = h.device_with_push("폰").await;
    h.start(CLAUDE, "s1").await;
    let deadline = T0 + 60_000;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["전체 중단", "중복 행만 건너뛰기"])],
            Some(deadline),
        )
        .await;
    let v = version_of(&h, &id).await;
    // 1ms 전에는 열려 있다
    h.clock.set(deadline - 1);
    assert_eq!(h.counts().await.1, 1);
    // 서버 시각 = 마감 → 접수 거절
    h.clock.set(deadline);
    let err = h
        .svc
        .submit(
            None,
            &id,
            &submit_req("sub-0000021", v, answers(&[(QD, "전체 중단")])),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict_code(err), "not_open");
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.closed_reason, Some(CloseReason::Expired));
    assert!(!d.answerable);
    assert_eq!(h.counts().await.1, 0, "대기 수에 남지 않음");
    // 오래된 푸시는 보내지 않는다
    assert_eq!(
        h.svc.process_push_outbox(&h.push, 10).await.unwrap().sent,
        0
    );
    assert_eq!(h.outbox_states().await, vec!["cancelled"]);
}

#[tokio::test]
async fn t20_deadline_or_withdrawal_before_delivery_stops_delivery() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    h.start(CLAUDE, "s2").await;
    let deadline = T0 + 60_000;
    let a = h
        .decide("s1", "ra", vec![q(QD, &["가", "나"])], Some(deadline))
        .await;
    let b = h.decide("s2", "rb", vec![q(QD, &["가", "나"])], None).await;
    let (va, vb) = (version_of(&h, &a).await, version_of(&h, &b).await);
    h.svc
        .submit(
            None,
            &a,
            &submit_req("sub-a-00020", va, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    h.svc
        .submit(
            None,
            &b,
            &submit_req("sub-b-00020", vb, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    // 전달 직전에 a는 마감, b는 철회
    h.clock.set(deadline);
    h.send(ev(
        CLAUDE,
        "s2",
        EventKind::DecisionWithdrawn {
            request_id: "rb".into(),
        },
    ))
    .await;
    h.svc.deliver_pending().await.unwrap();
    assert!(h.provider.delivered().is_empty(), "전달이 시작되지 않음");
    let da = h.svc.decision_detail(&a).await.unwrap();
    let db = h.svc.decision_detail(&b).await.unwrap();
    assert_eq!(da.closed_reason, Some(CloseReason::Expired));
    assert_eq!(db.closed_reason, Some(CloseReason::Withdrawn));
    assert_eq!(
        da.latest_submission.unwrap().state,
        SubmissionState::Cancelled
    );
    assert_eq!(
        db.latest_submission.unwrap().state,
        SubmissionState::Cancelled
    );
    assert_eq!(h.counts().await.1, 0);
}

// ---------- 보조 시험 ----------

#[tokio::test]
async fn answers_are_validated_against_questions() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q(QD, &["가", "나"]), q("두 번째?", &["예", "아니요"])],
            None,
        )
        .await;
    let v = version_of(&h, &id).await;
    let cases = [
        answers(&[(QD, "가")]),                                 // 질문 하나 빠짐
        answers(&[(QD, "다"), ("두 번째?", "예")]),             // 없는 선택지(자유 입력 불가)
        answers(&[(QD, "가"), ("두 번째?", "예"), ("x", "y")]), // 모르는 질문
    ];
    for (i, a) in cases.into_iter().enumerate() {
        let r = h
            .svc
            .submit(None, &id, &submit_req(&format!("sub-bad-{i:04}"), v, a))
            .await;
        assert!(matches!(r, Err(ServiceError::Invalid(_))), "경우 {i}");
    }
    let mut many = answers(&[("두 번째?", "예")]);
    many.insert(QD.into(), AnswerValue::Many(vec!["가".into()]));
    assert!(matches!(
        h.svc
            .submit(None, &id, &submit_req("sub-many-01", v, many))
            .await,
        Err(ServiceError::Invalid(_))
    ));
    assert_eq!(
        h.svc.decision_detail(&id).await.unwrap().summary.state,
        DecisionState::Open,
        "실패한 제출은 상태를 바꾸지 않음"
    );
}

#[tokio::test]
async fn free_text_is_accepted_when_allowed() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let e = ev(
        CLAUDE,
        "s1",
        EventKind::DecisionRequested {
            request_id: "r1".into(),
            title: None,
            questions: vec![q("파일 이름은?", &[])],
            allow_free_text: true,
            rationale: Some("이름 규칙이 없어서".into()),
            deadline_at: None,
        },
    );
    let id = h.svc.ingest(&e).await.unwrap().new_decision.unwrap();
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req(
                "sub-free-001",
                v,
                answers(&[("파일 이름은?", "report-final")]),
            ),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    assert_eq!(
        h.provider.delivered()[0].1,
        answers(&[("파일 이름은?", "report-final")])
    );
}

#[tokio::test]
async fn answered_elsewhere_and_session_end_clear_decisions() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    h.start(CLAUDE, "s2").await;
    let a = h.decide("s1", "ra", vec![q(QD, &["가", "나"])], None).await;
    let b = h.decide("s2", "rb", vec![q(QD, &["가", "나"])], None).await;
    h.send(ev(
        CLAUDE,
        "s1",
        EventKind::DecisionAnsweredElsewhere {
            request_id: "ra".into(),
        },
    ))
    .await;
    h.send(ev(
        CLAUDE,
        "s2",
        EventKind::SessionEnded {
            outcome: SessionOutcome::Interrupted,
        },
    ))
    .await;
    let da = h.svc.decision_detail(&a).await.unwrap();
    assert_eq!(
        (da.summary.state, da.closed_reason),
        (DecisionState::Applied, Some(CloseReason::Elsewhere))
    );
    let db = h.svc.decision_detail(&b).await.unwrap();
    assert_eq!(db.closed_reason, Some(CloseReason::SessionEnded));
    assert_eq!(h.counts().await, (1, 0), "s1은 재개, s2는 종료");
}

#[tokio::test]
async fn restart_turns_in_flight_delivery_into_confirming() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-restart1", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    // 전달 도중 프로세스가 죽은 상황을 DB로 재현
    sqlx::query("UPDATE submissions SET state = 'delivering'")
        .execute(h.svc.pool())
        .await
        .unwrap();
    assert_eq!(h.svc.recover_after_restart().await.unwrap(), 1);
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Confirming);
    assert_eq!(d.latest_submission.unwrap().state, SubmissionState::Unknown);
    // 원본이 아직 기다린다고 확인되면 다시 답할 수 있다
    h.provider.set_status("r1", RequestStatus::Open);
    h.svc.reconcile_confirming().await.unwrap();
    assert!(h.svc.decision_detail(&id).await.unwrap().answerable);
}

#[tokio::test]
async fn push_gone_removes_subscription_and_retry_backs_off() {
    let h = harness().await;
    let (_a, _) = h.device_with_push("폰A").await;
    let (_b, _) = h.device_with_push("폰B").await;
    h.start(CLAUDE, "s1").await;
    h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    h.push.push_outcome(PushOutcome::Gone);
    h.push.push_outcome(PushOutcome::Retry("503".into()));
    let st = h.svc.process_push_outbox(&h.push, 10).await.unwrap();
    assert_eq!((st.failed, st.retried), (1, 1));
    let devices = h.svc.list_devices().await.unwrap();
    assert_eq!(
        devices.iter().filter(|d| d.has_push).count(),
        1,
        "만료 구독은 지움"
    );
    // 백오프 전에는 다시 보내지 않음
    assert_eq!(
        h.svc.process_push_outbox(&h.push, 10).await.unwrap().sent,
        0
    );
    h.clock.advance(60_000);
    assert_eq!(
        h.svc.process_push_outbox(&h.push, 10).await.unwrap().sent,
        1
    );
}

#[tokio::test]
async fn stale_provider_marks_snapshot_degraded() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    assert!(!h.svc.snapshot().await.unwrap().degraded);
    h.clock.advance(120_001);
    let snap = h.svc.snapshot().await.unwrap();
    assert!(snap.degraded && snap.providers[0].stale);
    assert_eq!(
        snap.running, 1,
        "연결이 늦어도 0으로 바꾸지 않고 마지막 값을 유지"
    );
    h.send(ev(CLAUDE, "s1", EventKind::Heartbeat)).await;
    assert!(!h.svc.snapshot().await.unwrap().degraded);
}

#[tokio::test]
async fn provider_without_client_rejects_back_to_open() {
    let h = harness().await;
    h.start(CODEX, "x1").await;
    let e = ev(
        CODEX,
        "x1",
        EventKind::DecisionRequested {
            request_id: "r1".into(),
            title: None,
            questions: vec![q(QD, &["가", "나"])],
            allow_free_text: false,
            rationale: None,
            deadline_at: None,
        },
    );
    let id = h.svc.ingest(&e).await.unwrap().new_decision.unwrap();
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-codex-01", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert!(d.answerable);
    assert_eq!(
        d.latest_submission.unwrap().detail.as_deref(),
        Some("no_provider")
    );
}

// ---------- 코드 리뷰 반영: 전달 중 경쟁·시간 제한·확인 포기·세션 수 ----------

/// 전달을 붙잡아 둔 채 `during`을 실행하고, 풀어 준 뒤 전달 결과까지 기다린다.
async fn deliver_while<F, Fut>(h: &H, during: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (entered, release) = h.provider.hold_next();
    let svc = h.svc.clone();
    let task = tokio::spawn(async move { svc.deliver_pending().await.unwrap() });
    entered.notified().await;
    during().await;
    release.notify_one();
    task.await.unwrap();
}

#[tokio::test]
async fn answered_elsewhere_during_delivery_is_not_overwritten() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-race-001", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    // 전달 중에 맥에서 먼저 답이 반영됨 → 이후 우리 전달이 '수락'으로 돌아와도 덮어쓰지 않는다
    deliver_while(&h, || async {
        h.send(ev(
            CLAUDE,
            "s1",
            EventKind::DecisionAnsweredElsewhere {
                request_id: "r1".into(),
            },
        ))
        .await;
    })
    .await;
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.closed_reason, Some(CloseReason::Elsewhere));
    let s = d.latest_submission.unwrap();
    assert_eq!(
        (s.state, s.detail.as_deref()),
        (SubmissionState::Cancelled, Some("elsewhere"))
    );
}

#[tokio::test]
async fn unknown_result_after_withdrawal_does_not_linger() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-race-002", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    h.provider
        .push_outcome(DeliveryOutcome::Unknown("연결 끊김".into()));
    deliver_while(&h, || async {
        h.send(ev(
            CLAUDE,
            "s1",
            EventKind::DecisionWithdrawn {
                request_id: "r1".into(),
            },
        ))
        .await;
    })
    .await;
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.closed_reason, Some(CloseReason::Withdrawn));
    assert_eq!(
        d.latest_submission.unwrap().state,
        SubmissionState::Cancelled,
        "unknown으로 남지 않음"
    );
    assert_eq!(h.counts().await.1, 0);
}

#[tokio::test]
async fn provider_timeout_becomes_unknown() {
    let h = harness_with(|s| s.with_provider_timeout(std::time::Duration::from_millis(50))).await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-timeout1", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    let (_entered, _release_never) = h.provider.hold_next();
    h.svc.deliver_pending().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.summary.state, DecisionState::Confirming);
    let s = d.latest_submission.unwrap();
    assert_eq!(
        (s.state, s.detail.as_deref()),
        (SubmissionState::Unknown, Some("timeout"))
    );
}

#[tokio::test]
async fn long_unconfirmed_decision_is_closed() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.provider
        .push_outcome(DeliveryOutcome::Unknown("응답 유실".into()));
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-giveup-1", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    // 원본 상태를 계속 모름: 1시간 안에는 확인 중으로 남는다
    h.clock.advance(3_599_000);
    h.svc.reconcile_confirming().await.unwrap();
    assert_eq!(h.counts().await.1, 1);
    h.clock.advance(1_000);
    h.svc.reconcile_confirming().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.closed_reason, Some(CloseReason::Unconfirmed));
    assert_eq!(
        d.latest_submission.unwrap().state,
        SubmissionState::Cancelled
    );
    assert_eq!(h.counts().await.1, 0, "대기 수에 영원히 남지 않음");
}

#[tokio::test]
async fn reconcile_gone_closes_decision() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    h.provider
        .push_outcome(DeliveryOutcome::Unknown("응답 유실".into()));
    h.svc
        .submit(
            None,
            &id,
            &submit_req("sub-gone-001", v, answers(&[(QD, "가")])),
        )
        .await
        .unwrap();
    h.svc.deliver_pending().await.unwrap();
    h.provider.set_status("r1", RequestStatus::Gone);
    h.svc.reconcile_confirming().await.unwrap();
    let d = h.svc.decision_detail(&id).await.unwrap();
    assert_eq!(d.closed_reason, Some(CloseReason::Gone));
    assert_eq!(
        d.latest_submission.unwrap().state,
        SubmissionState::Cancelled
    );
}

#[tokio::test]
async fn session_with_open_decision_is_not_counted_running() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    // 결정 대기 중에 턴 시작·권한 해제 신호가 와도 진행 수에 넣지 않는다(설계 2.3)
    h.start(CLAUDE, "s1").await;
    h.send(ev(
        CLAUDE,
        "s1",
        EventKind::PermissionRequested { detail: None },
    ))
    .await;
    h.send(ev(CLAUDE, "s1", EventKind::PermissionResolved))
        .await;
    assert_eq!(h.counts().await, (0, 1));
    // 요청이 철회되면 원본 작업이 이어지므로 진행 중으로 돌아간다
    h.send(ev(
        CLAUDE,
        "s1",
        EventKind::DecisionWithdrawn {
            request_id: "r1".into(),
        },
    ))
    .await;
    assert_eq!(h.counts().await, (1, 0));
}

#[tokio::test]
async fn concurrent_duplicate_submissions_create_one() {
    let h = harness().await;
    h.start(CLAUDE, "s1").await;
    let id = h.decide("s1", "r1", vec![q(QD, &["가", "나"])], None).await;
    let v = version_of(&h, &id).await;
    let req = submit_req("sub-concur-1", v, answers(&[(QD, "가")]));
    let other = submit_req("sub-concur-2", v, answers(&[(QD, "나")]));
    let (a, b, c) = tokio::join!(
        h.svc.submit(None, &id, &req),
        h.svc.submit(None, &id, &req),
        h.svc.submit(None, &id, &other),
    );
    let created = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Ok((_, true))))
        .count();
    assert_eq!(created, 1, "같은 제출 ID는 한 번만 만들어진다");
    assert!(
        matches!(
            c,
            Err(ServiceError::Conflict {
                code: "not_open",
                ..
            })
        ),
        "다른 답은 거절"
    );
    h.svc.deliver_pending().await.unwrap();
    assert_eq!(h.provider.delivered().len(), 1);
}
