//! 시계. 시험에서는 고정·수동 시계로 마감 경계를 정확히 재현한다.

use chrono::{DateTime, TimeZone, Utc};
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    /// 현재 UTC 밀리초.
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

/// 시험용 수동 시계.
pub struct ManualClock(Mutex<i64>);

impl ManualClock {
    pub fn new(ms: i64) -> Self {
        Self(Mutex::new(ms))
    }
    pub fn set(&self, ms: i64) {
        *self.0.lock().unwrap() = ms;
    }
    pub fn advance(&self, ms: i64) {
        *self.0.lock().unwrap() += ms;
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        *self.0.lock().unwrap()
    }
}

/// 밀리초 → RFC 3339 표시용 시각.
pub fn to_datetime(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_default()
}
