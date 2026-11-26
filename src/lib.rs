//! AI 작업 상태·결정 대기 알림 서비스.
//!
//! 구성
//! - `service`: 상태 계산·전이(세션, 결정, 답변 제출, 마감, 푸시 대기열, 기기)
//! - `api`: 폰(PWA)이 쓰는 HTTP API
//! - `provider`: 제공자(Claude Code·Codex) 연결 계약과 시험용 가짜 제공자
//!
//! 설계는 `docs/design.md`를 따른다.

pub mod api;
pub mod claude_answer;
pub mod clock;
pub mod collect;
pub mod db;
pub mod model;
pub mod provider;
pub mod service;
