//! 도메인 타입: 세션·결정·제출 상태, 질문·답변, 제공자 이벤트.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

str_enum!(SessionState {
    Running => "running",
    Idle => "idle",
    WaitingDecision => "waiting_decision",
    WaitingPermission => "waiting_permission",
    Completed => "completed",
    Interrupted => "interrupted",
    Failed => "failed",
});

impl SessionState {
    /// 끝난 세션(완료·중단·실패).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

str_enum!(DecisionState {
    Open => "open",
    Sending => "sending",
    Confirming => "confirming",
    Applied => "applied",
    Closed => "closed",
});

impl DecisionState {
    /// 결정 대기 수에 포함되는 상태(설계 2.3절 표).
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Open | Self::Sending | Self::Confirming)
    }
}

str_enum!(CloseReason {
    ThisApp => "this_app",
    Elsewhere => "elsewhere",
    Withdrawn => "withdrawn",
    Expired => "expired",
    SessionEnded => "session_ended",
    Gone => "gone",
    Unconfirmed => "unconfirmed",
});

str_enum!(SubmissionState {
    Queued => "queued",
    Delivering => "delivering",
    Applied => "applied",
    Rejected => "rejected",
    Unknown => "unknown",
    Cancelled => "cancelled",
});

impl SubmissionState {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Applied | Self::Rejected | Self::Cancelled)
    }
}

str_enum!(SessionOutcome {
    Completed => "completed",
    Interrupted => "interrupted",
    Failed => "failed",
});

impl SessionOutcome {
    pub fn state(self) -> SessionState {
        match self {
            Self::Completed => SessionState::Completed,
            Self::Interrupted => SessionState::Interrupted,
            Self::Failed => SessionState::Failed,
        }
    }
}

/// 선택지 하나. Claude Code AskUserQuestion의 options 항목과 같은 모양.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 질문 하나. Claude Code AskUserQuestion의 questions 항목과 같은 모양.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
}

/// 질문 하나의 답: 단일 선택·자유 입력은 문자열, 다중 선택은 배열.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnswerValue {
    One(String),
    Many(Vec<String>),
}

/// 질문 전문 → 답. BTreeMap이라 직렬화 결과가 결정적이다(같은 입력 판정에 씀).
pub type Answers = BTreeMap<String, AnswerValue>;

/// 제공자 어댑터가 서비스에 넘기는 이벤트.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEvent {
    /// 제공자 이름(예: "claude-code").
    pub provider: String,
    /// 제공자의 원본 세션 ID.
    pub session_id: String,
    /// 이벤트 멱등 키. 같은 키는 한 번만 반영한다.
    pub event_id: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// 세션이 생겼다(아직 작업 전: 유휴).
    SessionStarted {
        title: String,
    },
    /// 턴(작업)이 시작되거나 재개됐다.
    TurnStarted,
    /// 턴이 끝나 다음 지시를 기다린다(유휴).
    TurnEnded,
    /// 서브 에이전트 활동: 상위 세션에 포함하므로 수를 바꾸지 않는다.
    SubagentActivity,
    /// 실행 권한 승인을 기다린다(프로젝트 결정 아님).
    PermissionRequested {
        #[serde(default)]
        detail: Option<String>,
    },
    PermissionResolved,
    /// 프로젝트 결정 요청.
    DecisionRequested {
        request_id: String,
        #[serde(default)]
        title: Option<String>,
        questions: Vec<Question>,
        #[serde(default)]
        allow_free_text: bool,
        #[serde(default)]
        rationale: Option<String>,
        /// UTC 밀리초.
        #[serde(default)]
        deadline_at: Option<i64>,
    },
    DecisionWithdrawn {
        request_id: String,
    },
    /// 맥 등 다른 곳에서 답이 반영됐다.
    DecisionAnsweredElsewhere {
        request_id: String,
    },
    SessionEnded {
        outcome: SessionOutcome,
    },
    /// 연결이 살아 있음(갱신 지연 판단용).
    Heartbeat,
}

/// 원래 요청에 답을 넣은 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// 원본이 답을 받아들였다.
    Accepted,
    /// 원본이 반영하지 않았음을 명시했다(재시도 가능).
    NotApplied(String),
    /// 반영 여부를 알 수 없다(연결 단절·응답 유실). 자동 재전송하지 않는다.
    Unknown(String),
}

/// 원본 요청의 현재 상태(반영 확인 중 정리에 씀).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    /// 아직 답을 기다린다 = 우리 답은 반영되지 않았다.
    Open,
    /// 답이 반영됐다.
    Answered,
    /// 요청이 사라졌다(세션 종료 등).
    Gone,
    /// 지금은 알 수 없다.
    Unknown,
}
