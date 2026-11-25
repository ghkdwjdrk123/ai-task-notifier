//! 실제 제공자 상태 수집.
//!
//! - Claude Code 세션: Claude Code 훅 입력(JSON)을 이벤트로 바꾼다(`notifier hook claude`).
//! - Claude 사용량: `claude -p /usage` 출력(모델 호출 없음)을 읽는다.
//! - Codex 세션·사용량: `~/.codex/sessions/**/rollout-*.jsonl`에 새로 붙는 줄을 읽는다.
//!
//! 대화 내용(프롬프트·답변)은 읽거나 저장하지 않는다. 세션 제목은 작업 폴더 이름만 쓴다.

use crate::model::{EventKind, ProviderEvent, SessionOutcome, SessionState, UsageWindow};
use crate::service::Service;
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const CLAUDE: &str = "claude-code";
pub const CODEX: &str = "codex";

/// 작업 폴더 경로 → 세션 제목(마지막 폴더 이름).
pub fn title_from_cwd(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "세션".into())
}

// ---------- Claude Code 훅 ----------

/// 훅 입력 하나를 어떻게 처리할지.
#[derive(Debug, Clone, PartialEq)]
pub enum HookAction {
    /// 이 이벤트들을 넣는다.
    Ingest(Vec<EventKind>),
    /// 도구 실행이 끝남: 권한·입력 대기 중이었으면 해제, 아니면 가끔 관측 시각만 갱신.
    ToolFinished,
    Ignore,
}

/// Claude Code 훅 입력 → 할 일. `session_id`가 없으면 무시.
pub fn claude_hook_action(input: &Value) -> (Option<String>, HookAction) {
    let sid = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or("");
    let ev = input
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let action = match ev {
        "SessionStart" => HookAction::Ingest(vec![EventKind::SessionStarted {
            title: title_from_cwd(cwd),
        }]),
        "UserPromptSubmit" => HookAction::Ingest(vec![EventKind::TurnStarted]),
        "Stop" | "StopFailure" => HookAction::Ingest(vec![EventKind::TurnEnded]),
        "SessionEnd" => HookAction::Ingest(vec![EventKind::SessionEnded {
            outcome: SessionOutcome::Completed,
        }]),
        "PermissionRequest" => HookAction::Ingest(vec![EventKind::PermissionRequested {
            detail: Some("권한".into()),
        }]),
        "Notification" => match input.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") | Some("elicitation_dialog") => {
                HookAction::Ingest(vec![EventKind::PermissionRequested {
                    detail: Some("권한".into()),
                }])
            }
            // 입력을 기다림(Esc로 멈춘 경우 Stop 훅이 없어 이것으로 유휴 처리)
            Some("idle_prompt") => HookAction::Ingest(vec![EventKind::TurnEnded]),
            _ => HookAction::Ignore,
        },
        "PreToolUse"
            if input.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") =>
        {
            HookAction::Ingest(vec![EventKind::PermissionRequested {
                detail: Some("질문".into()),
            }])
        }
        "PostToolUse" | "PostToolUseFailure" => HookAction::ToolFinished,
        _ => HookAction::Ignore,
    };
    (sid, action)
}

/// `notifier hook claude`: 표준 입력의 훅 JSON 하나를 반영한다.
/// Claude Code를 방해하지 않도록 호출한 쪽에서 오류를 삼키고 항상 0으로 끝낸다.
pub async fn run_claude_hook(svc: &Service, input: &str) -> Result<()> {
    let v: Value = serde_json::from_str(input).context("훅 입력이 JSON이 아닙니다")?;
    let (Some(sid), action) = claude_hook_action(&v) else {
        return Ok(());
    };
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
    let brief = svc.session_brief(CLAUDE, &sid).await?;
    let mut kinds = match action {
        HookAction::Ignore => return Ok(()),
        HookAction::Ingest(k) => k,
        HookAction::ToolFinished => match &brief {
            // 도구 호출마다 DB에 쓰지 않도록: 대기 중이면 해제, 아니면 10분에 한 번만 관측 시각 갱신
            Some(b) if b.state == SessionState::WaitingPermission => {
                vec![EventKind::PermissionResolved]
            }
            Some(b) if svc.now() - b.updated_at > 10 * 60_000 => vec![EventKind::SubagentActivity],
            Some(_) => return Ok(()),
            // 훅을 켜기 전에 시작한 세션: 도구가 돌고 있으니 진행 중으로 등록
            None => vec![EventKind::TurnStarted],
        },
    };
    // 처음 보는 세션이거나 제목이 아직 ID 조각이면 작업 폴더 이름을 제목으로
    let placeholder: String = sid.chars().take(12).collect();
    let needs_title = brief.as_ref().is_none_or(|b| b.title == placeholder);
    if needs_title
        && !cwd.is_empty()
        && !matches!(kinds.first(), Some(EventKind::SessionStarted { .. }))
    {
        kinds.insert(
            0,
            EventKind::SessionStarted {
                title: title_from_cwd(cwd),
            },
        );
    }
    for kind in kinds {
        let ev = ProviderEvent {
            provider: CLAUDE.into(),
            session_id: sid.clone(),
            event_id: format!("hook:{}", uuid::Uuid::new_v4().simple()),
            kind,
        };
        svc.ingest(&ev).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

// ---------- Claude 사용량(/usage 출력) ----------

/// `/usage` 출력 줄에서 사용량 창을 뽑는다.
/// 예: "Current session: 21% used · resets Nov 26 at 7:49am (Asia/Seoul)"
///     "Current week (all models): 42% used · resets Dec 1 at 8:59pm (Asia/Seoul)"
/// 초기화 시각은 이 맥의 현지 시간대로 해석한다(CLI가 현지 시간대로 출력).
pub fn parse_claude_usage(text: &str, now: DateTime<Local>) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("Current ") else {
            continue;
        };
        let Some((label, after)) = rest.split_once(':') else {
            continue;
        };
        let name = window_name(label.trim());
        let Some(name) = name else { continue };
        let after = after.trim();
        let Some(pct_end) = after.find('%') else {
            continue;
        };
        let raw = after[..pct_end].trim();
        // "<1% used" 같은 값은 0으로
        let Ok(pct) = raw
            .trim_start_matches('<')
            .trim()
            .parse::<f64>()
            .map(|v| if raw.starts_with('<') { 0.0 } else { v })
        else {
            continue;
        };
        if !(0.0..=100.0).contains(&pct) {
            continue;
        }
        // 출력의 시간대(괄호)가 이 맥의 시간대와 다르면 초기화 시각을 비운다(틀린 시각보다 없는 게 낫다)
        let resets_at = after.split_once("resets ").and_then(|(_, r)| {
            let (when, zone) = match r.split_once(" (") {
                Some((w, z)) => (w.trim(), Some(z.trim_end_matches(')').trim())),
                None => (r.trim(), None),
            };
            if let (Some(z), Some(local)) = (zone, local_zone_name())
                && z != local
            {
                return None;
            }
            parse_reset(when, now).map(|t| t.timestamp_millis())
        });
        out.push(UsageWindow {
            name,
            used_percent: pct,
            resets_at,
            status: None,
            observed_at: None,
        });
    }
    out
}

/// 이 맥의 IANA 시간대 이름(TZ 환경 변수 또는 /etc/localtime 링크). 모르면 None(검사 생략).
fn local_zone_name() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').to_string();
        if tz.contains('/') {
            return Some(tz);
        }
    }
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let s = link.to_string_lossy();
    s.split("zoneinfo/").nth(1).map(str::to_string)
}

fn window_name(label: &str) -> Option<String> {
    if label == "session" {
        return Some("five_hour".into());
    }
    let inner = label.strip_prefix("week")?.trim();
    if inner.is_empty() || inner == "(all models)" {
        return Some("seven_day".into());
    }
    let model: String = inner
        .trim_matches(|c| c == '(' || c == ')')
        .split_whitespace()
        .next()?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    (!model.is_empty()).then(|| format!("seven_day_{model}"))
}

/// "Nov 26 at 7:49am", "Dec 1 at 8pm", "7:49am"(오늘) → 현지 시각. 지난 시각이면 다음 해(날짜 있음)·다음 날(시각만).
fn parse_reset(s: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let (date_part, time_part) = match s.split_once(" at ") {
        Some((d, t)) => (Some(d.trim()), t.trim()),
        None => (None, s.trim()),
    };
    let t = time_part.to_ascii_lowercase();
    let (clock, pm) = if let Some(c) = t.strip_suffix("pm") {
        (c, true)
    } else {
        let c = t.strip_suffix("am")?;
        (c, false)
    };
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&h) {
        return None;
    }
    let hour = (h % 12) + if pm { 12 } else { 0 };
    let time = NaiveTime::from_hms_opt(hour, m, 0)?;
    match date_part {
        Some(d) => {
            let mut it = d.split_whitespace();
            let month = month_number(it.next()?)?;
            let day: u32 = it.next()?.trim_end_matches(',').parse().ok()?;
            let mut year = now.year();
            let mut date = NaiveDate::from_ymd_opt(year, month, day)?;
            if Local.from_local_datetime(&date.and_time(time)).single()?
                < now - chrono::Duration::days(1)
            {
                year += 1;
                date = NaiveDate::from_ymd_opt(year, month, day)?;
            }
            Local.from_local_datetime(&date.and_time(time)).earliest()
        }
        None => {
            let today = Local
                .from_local_datetime(&now.date_naive().and_time(time))
                .earliest()?;
            Some(if today < now {
                today + chrono::Duration::days(1)
            } else {
                today
            })
        }
    }
}

fn month_number(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|x| m.starts_with(x))
    .map(|i| i as u32 + 1)
}

/// Claude CLI로 /usage를 한 번 읽는다(모델 호출 없음). 출력은 `--output-format json`의 result.
pub async fn read_claude_usage(
    claude_bin: &Path,
    config_dir: &Path,
    work_dir: &Path,
) -> Result<Vec<UsageWindow>> {
    std::fs::create_dir_all(work_dir).ok();
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(claude_bin)
            .args([
                "-p",
                "/usage",
                "--output-format",
                "json",
                "--setting-sources",
                "project",
                "--strict-mcp-config",
            ])
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
            .current_dir(work_dir)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("/usage 시간 초과")?
    .context("claude 실행 실패")?;
    let v: Value = serde_json::from_slice(&out.stdout).context("/usage 출력이 JSON이 아닙니다")?;
    let text = v.get("result").and_then(Value::as_str).unwrap_or("");
    let windows = parse_claude_usage(text, Local::now());
    if windows.is_empty() {
        anyhow::bail!("/usage 출력에서 사용량을 찾지 못했습니다(로그인 만료 또는 형식 변경)");
    }
    Ok(windows)
}

// ---------- Codex 기록 ----------

/// Codex 기록 파일 하나의 읽기 상태.
#[derive(Debug, Default)]
pub struct CodexFile {
    offset: u64,
    line_no: u64,
    session_id: Option<String>,
    awaiting_approval: bool,
}

/// Codex 기록 한 줄 → 이벤트. 세션 ID는 파일 첫 줄(session_meta)에서 얻는다.
pub fn codex_line_events(line: &Value, st: &mut CodexFile) -> Vec<EventKind> {
    let ty = line.get("type").and_then(Value::as_str).unwrap_or("");
    let p = line.get("payload").cloned().unwrap_or(Value::Null);
    if ty == "session_meta" {
        // 이어 붙은 기록(fork·resume)의 두 번째 session_meta로 세션 ID가 바뀌지 않게
        if st.session_id.is_none() {
            st.session_id = p.get("id").and_then(Value::as_str).map(str::to_string);
        }
        let cwd = p.get("cwd").and_then(Value::as_str).unwrap_or("");
        return vec![EventKind::SessionStarted {
            title: title_from_cwd(cwd),
        }];
    }
    if ty != "event_msg" {
        return vec![];
    }
    let pty = p.get("type").and_then(Value::as_str).unwrap_or("");
    match pty {
        "task_started" => vec![EventKind::TurnStarted],
        "task_complete" | "turn_aborted" => {
            st.awaiting_approval = false;
            vec![EventKind::TurnEnded]
        }
        t if t.ends_with("approval_request") || t == "request_user_input" => {
            st.awaiting_approval = true;
            vec![EventKind::PermissionRequested {
                detail: Some("승인".into()),
            }]
        }
        // 승인한 명령·패치가 시작·끝나면 대기 해제
        "exec_command_begin" | "exec_command_end" | "patch_apply_begin" | "patch_apply_end"
            if st.awaiting_approval =>
        {
            st.awaiting_approval = false;
            vec![EventKind::PermissionResolved]
        }
        "token_count" => {
            let mut out = vec![];
            if std::mem::take(&mut st.awaiting_approval) {
                // 승인 뒤 모델이 다시 응답함 = 대기 해제
                out.push(EventKind::PermissionResolved);
            }
            if let Some(rl) = p.get("rate_limits") {
                // 이 줄이 기록된 시각 = 관측 시각
                let observed_at = line
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.timestamp_millis());
                let windows: Vec<UsageWindow> = ["primary", "secondary"]
                    .iter()
                    .filter_map(|k| rl.get(*k))
                    .filter_map(|w| {
                        let pct = w.get("used_percent")?.as_f64()?;
                        let minutes = w.get("window_minutes").and_then(Value::as_i64).unwrap_or(0);
                        let name = match minutes {
                            300 => "five_hour".to_string(),
                            10080 => "seven_day".to_string(),
                            m => format!("window_{m}m"),
                        };
                        let resets_at =
                            w.get("resets_at").and_then(Value::as_i64).map(|s| s * 1000);
                        Some(UsageWindow {
                            name,
                            used_percent: pct.clamp(0.0, 100.0),
                            resets_at,
                            status: None,
                            observed_at,
                        })
                    })
                    .collect();
                if !windows.is_empty() {
                    out.push(EventKind::UsageReported { windows });
                }
            }
            out
        }
        _ => vec![],
    }
}

/// Codex 세션 폴더를 지켜본다. 최근 수정된 기록 파일의 새 줄만 읽어 이벤트로 넣는다.
pub struct CodexWatcher {
    root: PathBuf,
    files: HashMap<PathBuf, CodexFile>,
    bootstrapped: bool,
    last_heartbeat: Option<std::time::Instant>,
}

/// 이 시간 안에 바뀐 기록 파일만 읽는다(옛 세션은 무시).
const CODEX_RECENT: Duration = Duration::from_secs(6 * 3600);

impl CodexWatcher {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            root: codex_home.join("sessions"),
            files: HashMap::new(),
            bootstrapped: false,
            last_heartbeat: None,
        }
    }

    /// 가장 최근 기록 파일(나이 무관).
    fn newest_file(&self) -> Option<PathBuf> {
        let mut best: Option<(SystemTime, PathBuf)> = None;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                let Ok(meta) = e.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(path);
                } else if path.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("rollout-") && n.ends_with(".jsonl")
                }) {
                    let m = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    if best.as_ref().is_none_or(|(b, _)| m > *b) {
                        best = Some((m, path));
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// 시작 때 한 번: 최근 사용이 없어도 마지막으로 기록된 사용량을 '○시간 전' 값으로 보여 준다.
    async fn bootstrap_usage(&self, svc: &Service) -> Result<()> {
        let Some(path) = self.newest_file() else {
            return Ok(());
        };
        let f = std::io::BufReader::new(std::fs::File::open(&path)?);
        let mut last: Option<(usize, Vec<UsageWindow>)> = None;
        let mut scratch = CodexFile::default();
        for (i, line) in f.lines().enumerate() {
            let Ok(line) = line else { break };
            if !line.contains("\"rate_limits\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            for k in codex_line_events(&v, &mut scratch) {
                if let EventKind::UsageReported { windows } = k {
                    last = Some((i + 1, windows));
                }
            }
        }
        if let Some((line_no, windows)) = last {
            let ev = ProviderEvent {
                provider: CODEX.into(),
                session_id: "-".into(),
                event_id: format!(
                    "codex-boot:{}:{line_no}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                kind: EventKind::UsageReported { windows },
            };
            svc.ingest(&ev).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }

    fn recent_files(&self) -> Vec<PathBuf> {
        let mut out = vec![];
        let cutoff = SystemTime::now() - CODEX_RECENT;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                let Ok(meta) = e.metadata() else { continue };
                if meta.is_dir() {
                    // 폴더 수정 시각은 파일에 이어 쓸 때 바뀌지 않으므로(오래 도는 세션) 걸러 내지 않는다
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "jsonl")
                    && path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
                    && meta.modified().map(|m| m >= cutoff).unwrap_or(false)
                {
                    out.push(path);
                }
            }
        }
        out
    }

    async fn poll_file(&mut self, svc: &Service, path: &Path) -> Result<usize> {
        let st = self.files.entry(path.to_path_buf()).or_default();
        let lines = read_new_lines(path, st)?;
        let mut n = 0;
        for (line_no, text) in lines {
            // 대화 내용 줄은 해석하지 않는다(필요한 종류만)
            if !(text.contains("\"session_meta\"") || text.contains("\"event_msg\"")) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            for (i, kind) in codex_line_events(&v, st).into_iter().enumerate() {
                let Some(sid) = st.session_id.clone() else {
                    continue;
                };
                let ev = ProviderEvent {
                    provider: CODEX.into(),
                    session_id: sid,
                    // 파일·줄 번호로 멱등 키: 서버를 다시 켜도 같은 줄을 두 번 반영하지 않음
                    event_id: format!(
                        "codex:{}:{line_no}:{i}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    kind,
                };
                match svc.ingest(&ev).await {
                    Ok(_) => n += 1,
                    Err(e) => {
                        tracing::warn!("Codex 이벤트 반영 실패({}:{line_no}): {e}", path.display())
                    }
                }
            }
        }
        Ok(n)
    }

    /// 한 번 훑어 새 줄을 반영한다. 반영한 이벤트 수.
    pub async fn poll(&mut self, svc: &Service) -> Result<usize> {
        if !self.bootstrapped {
            self.bootstrapped = true;
            if let Err(e) = self.bootstrap_usage(svc).await {
                tracing::warn!("Codex 마지막 사용량 읽기 실패: {e:#}");
            }
        }
        let mut n = 0;
        let recent = self.recent_files();
        // 6시간 넘게 안 바뀐 파일의 읽기 상태는 버린다
        self.files.retain(|p, _| recent.contains(p));
        for path in recent {
            match self.poll_file(svc, &path).await {
                Ok(k) => n += k,
                // 한 파일의 오류가 나머지 파일·하트비트를 막지 않게
                Err(e) => tracing::warn!("Codex 기록 읽기 실패({}): {e:#}", path.display()),
            }
        }
        // 지켜보는 중이라는 신호(갱신 지연 판단용): 1분에 한 번, 멱등 키 없이
        if self
            .last_heartbeat
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
        {
            self.last_heartbeat = Some(std::time::Instant::now());
            svc.heartbeat(CODEX).await?;
        }
        Ok(n)
    }
}

/// 지난번 읽은 곳부터 완성된 줄(끝에 \n)만 읽는다. (줄 번호, 내용)
fn read_new_lines(path: &Path, st: &mut CodexFile) -> Result<Vec<(u64, String)>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < st.offset {
        // 파일이 줄었으면(교체) 처음부터
        *st = CodexFile::default();
    }
    f.seek(SeekFrom::Start(st.offset))?;
    let mut r = std::io::BufReader::new(f);
    let mut out = vec![];
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = r.read_until(b'\n', &mut buf)?;
        if read == 0 || buf.last() != Some(&b'\n') {
            break; // 아직 쓰는 중인 마지막 줄은 다음에
        }
        st.offset += read as u64;
        st.line_no += 1;
        // 아주 긴 줄(이미지 등)은 건너뛴다. 깨진 글자는 대체 문자로(오류로 멈추지 않게)
        if buf.len() <= MAX_LINE_BYTES {
            out.push((
                st.line_no,
                String::from_utf8_lossy(&buf).trim_end().to_string(),
            ));
        }
    }
    Ok(out)
}

/// 이보다 긴 기록 줄은 읽지 않는다(사용량·상태 줄은 몇 KB 이하).
const MAX_LINE_BYTES: usize = 1 << 20;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn parses_usage_text() {
        let text = "You are currently using your subscription to power your Claude Code usage\n\n\
                    Current session: 21% used · resets Nov 26 at 7:49am (Asia/Seoul)\n\
                    Current week (all models): 42% used · resets Dec 1 at 8:59pm (Asia/Seoul)\n\
                    Current week (Opus only): 3.5% used · resets Dec 1 at 9pm (Asia/Seoul)";
        let now = at("2026-11-26 03:00");
        let w = parse_claude_usage(text, now);
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].name.as_str(), w[0].used_percent), ("five_hour", 21.0));
        assert_eq!(
            w[0].resets_at,
            Some(at("2026-11-26 07:49").timestamp_millis())
        );
        assert_eq!((w[1].name.as_str(), w[1].used_percent), ("seven_day", 42.0));
        assert_eq!(
            w[1].resets_at,
            Some(at("2026-12-01 20:59").timestamp_millis())
        );
        assert_eq!(
            (w[2].name.as_str(), w[2].used_percent),
            ("seven_day_opus", 3.5)
        );
        assert_eq!(
            w[2].resets_at,
            Some(at("2026-12-01 21:00").timestamp_millis())
        );
    }

    #[test]
    fn reset_across_year_and_time_only() {
        let now = at("2026-12-30 10:00");
        assert_eq!(
            parse_reset("Jan 2 at 12am", now),
            Some(at("2027-01-02 00:00"))
        );
        assert_eq!(parse_reset("9am", now), Some(at("2026-12-31 09:00")));
        assert_eq!(parse_reset("11:30am", now), Some(at("2026-12-30 11:30")));
        assert!(parse_claude_usage("Not logged in", now).is_empty());
        // "<1%"는 0으로, 모르는 줄은 무시
        let w = parse_claude_usage(
            "Current session: <1% used · resets 9am\nSomething else: 5% used",
            now,
        );
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].name.as_str(), w[0].used_percent), ("five_hour", 0.0));
    }

    #[test]
    fn hook_mapping() {
        let base = |ev: &str| json!({"session_id": "s1", "cwd": "/Users/x/proj/my-app", "hook_event_name": ev});
        assert_eq!(
            claude_hook_action(&base("SessionStart")),
            (
                Some("s1".into()),
                HookAction::Ingest(vec![EventKind::SessionStarted {
                    title: "my-app".into()
                }])
            )
        );
        assert_eq!(
            claude_hook_action(&base("UserPromptSubmit")).1,
            HookAction::Ingest(vec![EventKind::TurnStarted])
        );
        assert_eq!(
            claude_hook_action(&base("StopFailure")).1,
            HookAction::Ingest(vec![EventKind::TurnEnded])
        );
        assert!(
            matches!(claude_hook_action(&base("SessionEnd")).1, HookAction::Ingest(v) if matches!(v[0], EventKind::SessionEnded { .. }))
        );
        assert!(
            matches!(claude_hook_action(&base("PermissionRequest")).1, HookAction::Ingest(v) if matches!(v[0], EventKind::PermissionRequested { .. }))
        );
        let mut perm = base("Notification");
        perm["notification_type"] = json!("permission_prompt");
        assert!(
            matches!(claude_hook_action(&perm).1, HookAction::Ingest(v) if matches!(v[0], EventKind::PermissionRequested { .. }))
        );
        assert_eq!(
            claude_hook_action(&base("Stop")).1,
            HookAction::Ingest(vec![EventKind::TurnEnded])
        );
        let mut ask = base("PreToolUse");
        ask["tool_name"] = json!("AskUserQuestion");
        assert!(
            matches!(claude_hook_action(&ask).1, HookAction::Ingest(v) if matches!(v[0], EventKind::PermissionRequested { .. }))
        );
        let mut bash = base("PreToolUse");
        bash["tool_name"] = json!("Bash");
        assert_eq!(claude_hook_action(&bash).1, HookAction::Ignore);
        let mut idle = base("Notification");
        idle["notification_type"] = json!("idle_prompt");
        assert_eq!(
            claude_hook_action(&idle).1,
            HookAction::Ingest(vec![EventKind::TurnEnded])
        );
        let mut other = base("Notification");
        other["notification_type"] = json!("auth_success");
        assert_eq!(claude_hook_action(&other).1, HookAction::Ignore);
        assert_eq!(
            claude_hook_action(&base("PostToolUse")).1,
            HookAction::ToolFinished
        );
        assert_eq!(
            claude_hook_action(&json!({"hook_event_name": "Stop"})).0,
            None
        );
    }

    #[test]
    fn codex_lines() {
        let mut st = CodexFile::default();
        let meta = json!({"type": "session_meta", "payload": {"id": "c-1", "cwd": "/w/api-tests"}});
        assert_eq!(
            codex_line_events(&meta, &mut st),
            vec![EventKind::SessionStarted {
                title: "api-tests".into()
            }]
        );
        assert_eq!(st.session_id.as_deref(), Some("c-1"));
        let started = json!({"type": "event_msg", "payload": {"type": "task_started"}});
        assert_eq!(
            codex_line_events(&started, &mut st),
            vec![EventKind::TurnStarted]
        );
        let approval = json!({"type": "event_msg", "payload": {"type": "exec_approval_request"}});
        assert!(matches!(
            codex_line_events(&approval, &mut st)[0],
            EventKind::PermissionRequested { .. }
        ));
        let tc = json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
            "primary": {"used_percent": 35.0, "window_minutes": 300, "resets_at": 1795544023},
            "secondary": {"used_percent": 20.0, "window_minutes": 10080, "resets_at": 1795704389}}}});
        let evs = codex_line_events(&tc, &mut st);
        assert_eq!(evs[0], EventKind::PermissionResolved);
        let EventKind::UsageReported { windows } = &evs[1] else {
            panic!()
        };
        assert_eq!(
            (
                windows[0].name.as_str(),
                windows[0].used_percent,
                windows[0].resets_at
            ),
            ("five_hour", 35.0, Some(1795544023000))
        );
        assert_eq!(windows[1].name, "seven_day");
        let done = json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": "비밀"}});
        assert_eq!(
            codex_line_events(&done, &mut st),
            vec![EventKind::TurnEnded]
        );
        let msg = json!({"type": "response_item", "payload": {"type": "message"}});
        assert!(codex_line_events(&msg, &mut st).is_empty());
    }

    #[test]
    fn reads_only_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout-x.jsonl");
        std::fs::write(&p, "{\"a\":1}\n{\"b\":").unwrap();
        let mut st = CodexFile::default();
        let first = read_new_lines(&p, &mut st).unwrap();
        assert_eq!(first, vec![(1, "{\"a\":1}".to_string())]);
        std::fs::write(&p, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        assert_eq!(
            read_new_lines(&p, &mut st).unwrap(),
            vec![(2, "{\"b\":2}".to_string())]
        );
        assert!(read_new_lines(&p, &mut st).unwrap().is_empty());
    }
}
