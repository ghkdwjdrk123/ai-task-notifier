//! 푸시 발송 계약과 시험용 가짜 발송기.
//! 실제 Web Push(RFC 8291·8292) 발송기는 2단계에서 붙인다.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

/// 브라우저 PushSubscription.toJSON()과 같은 모양.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushKeys,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Sent,
    /// 구독이 사라졌다(404·410): 구독을 지운다.
    Gone,
    /// 일시 실패: 나중에 다시 보낸다.
    Retry(String),
}

pub trait PushSender: Send + Sync {
    fn send<'a>(
        &'a self,
        sub: &'a PushSubscription,
        payload: &'a [u8],
        ttl_secs: u32,
    ) -> BoxFuture<'a, PushOutcome>;
}

/// 발송 한 건의 기록(시험 확인용).
#[derive(Debug, Clone)]
pub struct SentPush {
    pub endpoint: String,
    pub payload: serde_json::Value,
    pub ttl_secs: u32,
}

#[derive(Default)]
pub struct FakePushSender {
    outcomes: Mutex<VecDeque<PushOutcome>>,
    sent: Mutex<Vec<SentPush>>,
}

impl FakePushSender {
    pub fn new() -> Self {
        Self::default()
    }
    /// 다음 발송 결과를 예약한다. 없으면 Sent.
    pub fn push_outcome(&self, o: PushOutcome) {
        self.outcomes.lock().unwrap().push_back(o);
    }
    pub fn sent(&self) -> Vec<SentPush> {
        self.sent.lock().unwrap().clone()
    }
}

impl PushSender for FakePushSender {
    fn send<'a>(
        &'a self,
        sub: &'a PushSubscription,
        payload: &'a [u8],
        ttl_secs: u32,
    ) -> BoxFuture<'a, PushOutcome> {
        Box::pin(async move {
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(PushOutcome::Sent);
            if outcome == PushOutcome::Sent {
                self.sent.lock().unwrap().push(SentPush {
                    endpoint: sub.endpoint.clone(),
                    payload: serde_json::from_slice(payload).unwrap_or_default(),
                    ttl_secs,
                });
            }
            outcome
        })
    }
}
