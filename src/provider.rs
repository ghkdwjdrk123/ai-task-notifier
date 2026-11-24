//! 제공자 연결 계약과 시험용 가짜 제공자.

use crate::model::{Answers, DeliveryOutcome, Question, RequestStatus};
use futures_util::future::BoxFuture;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// 답을 넣을 원래 요청.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTarget {
    pub provider_session_id: String,
    pub provider_request_id: String,
    pub questions: Vec<Question>,
}

/// 제공자 어댑터가 구현한다(Claude Code는 2단계, Codex는 3단계).
///
/// 어댑터 계약(서비스가 이것을 전제로 상태를 계산한다)
/// - **이벤트 순서**: 같은 세션의 이벤트는 발생 순서대로 `Service::ingest`에 넘긴다.
///   서비스는 멱등 키로 중복만 거르고 순서를 다시 맞추지 않는다. 예를 들어 철회가 요청보다
///   먼저 오면 철회는 버려지고 요청은 열린 채로 남는다.
/// - **멱등 키**: 재전송해도 같은 이벤트면 같은 `event_id`를 쓴다.
/// - `deliver_answer`의 `NotApplied`는 원본이 답을 받지 않았음이 **확실할 때만** 쓴다.
///   모르면 `Unknown`. (`NotApplied`이면 사용자가 다시 보낼 수 있어 이중 반영 위험이 있다.)
/// - `request_status`의 `Open`도 우리 답이 반영되지 않았음이 확실할 때만 쓴다.
///   `Answered`는 누가 답했든 반영됐다는 뜻이다(서비스는 이 앱의 답으로 기록한다).
/// - 두 호출은 서비스가 시간 제한(기본 30초)을 건다. 넘기면 `Unknown`으로 본다.
pub trait ProviderClient: Send + Sync {
    /// 원래 요청에 답을 넣는다. 이 호출은 DB 트랜잭션 밖에서 불린다.
    fn deliver_answer<'a>(
        &'a self,
        target: &'a DeliveryTarget,
        answers: &'a Answers,
    ) -> BoxFuture<'a, DeliveryOutcome>;

    /// 원래 요청이 아직 열려 있는지, 답이 반영됐는지 조회한다.
    fn request_status<'a>(&'a self, target: &'a DeliveryTarget) -> BoxFuture<'a, RequestStatus>;
}

/// 시험용 가짜 제공자: 정해 둔 결과를 차례로 돌려주고, 받은 답을 기록한다.
#[derive(Default)]
pub struct FakeProvider {
    outcomes: Mutex<VecDeque<DeliveryOutcome>>,
    statuses: Mutex<HashMap<String, RequestStatus>>,
    delivered: Mutex<Vec<(DeliveryTarget, Answers)>>,
    /// 다음 전달을 붙잡아 둘 관문(전달 중 경쟁 시험용).
    gate: Mutex<Option<Arc<Notify>>>,
    entered: Arc<Notify>,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// 다음 deliver_answer 결과를 예약한다. 예약이 없으면 Accepted.
    pub fn push_outcome(&self, o: DeliveryOutcome) {
        self.outcomes.lock().unwrap().push_back(o);
    }

    /// request_status 결과를 원본 요청 ID별로 정한다. 없으면 Unknown.
    pub fn set_status(&self, provider_request_id: &str, s: RequestStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(provider_request_id.to_string(), s);
    }

    /// 다음 deliver_answer를 붙잡는다. (전달 시작 알림, 풀어 주기) — 풀어 주지 않으면 끝나지 않는다.
    pub fn hold_next(&self) -> (Arc<Notify>, Arc<Notify>) {
        let release = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(release.clone());
        (self.entered.clone(), release)
    }

    /// 지금까지 넣은 답(요청, 답).
    pub fn delivered(&self) -> Vec<(DeliveryTarget, Answers)> {
        self.delivered.lock().unwrap().clone()
    }
}

impl ProviderClient for FakeProvider {
    fn deliver_answer<'a>(
        &'a self,
        target: &'a DeliveryTarget,
        answers: &'a Answers,
    ) -> BoxFuture<'a, DeliveryOutcome> {
        Box::pin(async move {
            self.delivered
                .lock()
                .unwrap()
                .push((target.clone(), answers.clone()));
            let gate = self.gate.lock().unwrap().take();
            if let Some(release) = gate {
                self.entered.notify_one();
                release.notified().await;
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DeliveryOutcome::Accepted)
        })
    }

    fn request_status<'a>(&'a self, target: &'a DeliveryTarget) -> BoxFuture<'a, RequestStatus> {
        Box::pin(async move {
            self.statuses
                .lock()
                .unwrap()
                .get(&target.provider_request_id)
                .cloned()
                .unwrap_or(RequestStatus::Unknown)
        })
    }
}
