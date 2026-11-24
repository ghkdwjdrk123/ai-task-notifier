//! 시험 공용 도구: 수동 시계·가짜 제공자·가짜 푸시 발송기를 붙인 서비스.
#![allow(dead_code)]

use ai_task_notifier::clock::ManualClock;
use ai_task_notifier::db;
use ai_task_notifier::model::{EventKind, ProviderEvent, Question, QuestionOption};
use ai_task_notifier::provider::FakeProvider;
use ai_task_notifier::push::{FakePushSender, PushKeys, PushSubscription};
use ai_task_notifier::service::Service;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::TempDir;

/// 시험 기준 시각(UTC 밀리초).
pub const T0: i64 = 1_800_000_000_000;
pub const CLAUDE: &str = "claude-code";
pub const CODEX: &str = "codex";

pub struct H {
    pub svc: Arc<Service>,
    pub clock: Arc<ManualClock>,
    pub provider: Arc<FakeProvider>,
    pub push: FakePushSender,
    _dir: TempDir,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

pub async fn harness() -> H {
    harness_with(|s| s).await
}

/// 서비스 설정(시간 제한 등)을 바꾼 시험 도구.
pub async fn harness_with(configure: impl FnOnce(Service) -> Service) -> H {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("t.db")).await.unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let provider = Arc::new(FakeProvider::new());
    let svc = configure(Service::new(pool, clock.clone()).with_provider(CLAUDE, provider.clone()));
    H {
        svc: Arc::new(svc),
        clock,
        provider,
        push: FakePushSender::new(),
        _dir: dir,
    }
}

/// 새 멱등 키를 가진 이벤트.
pub fn ev(provider: &str, session: &str, kind: EventKind) -> ProviderEvent {
    ProviderEvent {
        provider: provider.into(),
        session_id: session.into(),
        event_id: format!("e{}", SEQ.fetch_add(1, Ordering::Relaxed)),
        kind,
    }
}

pub fn q(text: &str, labels: &[&str]) -> Question {
    Question {
        question: text.into(),
        header: None,
        options: labels
            .iter()
            .map(|l| QuestionOption {
                label: (*l).into(),
                description: None,
            })
            .collect(),
        multi_select: false,
    }
}

pub fn decision_event(
    session: &str,
    request_id: &str,
    questions: Vec<Question>,
    deadline_at: Option<i64>,
) -> ProviderEvent {
    ev(
        CLAUDE,
        session,
        EventKind::DecisionRequested {
            request_id: request_id.into(),
            title: Some("데이터 가져오기".into()),
            questions,
            allow_free_text: false,
            rationale: None,
            deadline_at,
        },
    )
}

impl H {
    pub async fn send(&self, e: ProviderEvent) {
        self.svc.ingest(&e).await.unwrap();
    }

    pub async fn start(&self, provider: &str, session: &str) {
        self.send(ev(provider, session, EventKind::TurnStarted))
            .await;
    }

    /// 결정 요청을 보내고 새 결정 ID를 돌려준다.
    pub async fn decide(
        &self,
        session: &str,
        request_id: &str,
        questions: Vec<Question>,
        deadline_at: Option<i64>,
    ) -> String {
        let r = self
            .svc
            .ingest(&decision_event(session, request_id, questions, deadline_at))
            .await
            .unwrap();
        r.new_decision.expect("새 결정")
    }

    pub async fn counts(&self) -> (i64, i64) {
        let s = self.svc.snapshot().await.unwrap();
        (s.running, s.waiting_decisions)
    }

    /// 푸시 구독까지 마친 기기 하나를 등록한다. (기기 ID, 토큰)
    pub async fn device_with_push(&self, name: &str) -> (String, String) {
        let code = self.svc.create_pairing_code(600_000).await.unwrap();
        let (d, token) = self.svc.exchange_pairing_code(&code, name).await.unwrap();
        self.svc
            .set_push_subscription(&d.id, &subscription(name))
            .await
            .unwrap();
        (d.id, token)
    }

    pub async fn outbox_states(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT state FROM push_outbox ORDER BY id")
            .fetch_all(self.svc.pool())
            .await
            .unwrap()
    }
}

/// 형식이 올바른 가짜 구독(p256dh 65바이트, auth 16바이트).
pub fn subscription(tag: &str) -> PushSubscription {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut key = vec![4u8];
    key.extend([7u8; 64]);
    PushSubscription {
        endpoint: format!("https://fcm.googleapis.com/fcm/send/test-{tag}"),
        keys: PushKeys {
            p256dh: b64.encode(&key),
            auth: b64.encode([9u8; 16]),
        },
    }
}
