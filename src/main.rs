//! notifier 명령: 서비스 실행, 기기 등록 코드 발급, 기기 목록·해제, 이벤트 넣기.

use ai_task_notifier::api::{self, ApiConfig, AppState};
use ai_task_notifier::claude_answer;
use ai_task_notifier::clock::SystemClock;
use ai_task_notifier::collect;
use ai_task_notifier::db;
use ai_task_notifier::model::ProviderEvent;
use ai_task_notifier::provider::FakeProvider;
use ai_task_notifier::service::Service;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Parser)]
#[command(name = "notifier", about = "AI 작업 상태·결정 대기 알림 서비스")]
struct Cli {
    /// DB 파일 경로(기본: ~/.local/share/ai-task-notifier/notifier.db)
    #[arg(long, env = "NOTIFIER_DB", global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// API 서버와 작업자(마감·답 전달·반영 확인)를 실행한다.
    Serve {
        /// 루프백 주소만 허용(폰은 Tailscale Serve 등으로 들어온다).
        #[arg(long, default_value = "127.0.0.1:8770")]
        listen: SocketAddr,
        /// 브라우저 쓰기 요청에 허용할 Origin. 여러 번 줄 수 있다(Android 앱만 쓰면 생략).
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
        /// 쿠키에서 Secure를 뺀다(HTTPS 없이 로컬에서 시험할 때만).
        #[arg(long)]
        insecure_cookie: bool,
        /// 시험용: 이 이름의 제공자에 대한 답을 바로 '수락'으로 처리하는 가짜 제공자를 붙인다.
        #[arg(long)]
        demo_provider: Option<String>,
        /// Claude 사용량을 `claude -p /usage`로 5분마다 읽는다(모델 호출 없음). 이 설정 폴더의 로그인을 쓴다.
        #[arg(long, value_name = "CLAUDE_CONFIG_DIR")]
        claude_usage: Option<PathBuf>,
        /// claude 실행 파일 경로(LaunchAgent처럼 PATH가 짧을 때 절대 경로로).
        #[arg(long, default_value = "claude")]
        claude_bin: PathBuf,
        /// Codex 기록 폴더를 지켜본다(보통 ~/.codex).
        #[arg(long, value_name = "CODEX_HOME")]
        codex: Option<PathBuf>,
    },
    /// Claude Code 훅에서 부른다: 표준 입력의 훅 JSON을 반영한다. 항상 0으로 끝난다. 표준 출력에는 PermissionRequest에 폰 답이 왔을 때의 결정 JSON만 쓴다(`--wait-secs`초까지 기다림).
    Hook {
        /// 지금은 "claude"만.
        provider: String,
    },
    /// 표준 입력의 제공자 이벤트(JSON 한 줄에 하나)를 넣는다. 훅·시험용.
    Ingest,
    /// 폰 등록용 1회용 코드를 만든다.
    Pair {
        /// 유효 시간(분).
        #[arg(long, default_value_t = 10)]
        ttl_min: i64,
    },
    /// 등록된 기기 목록.
    Devices,
    /// 기기 연결을 해제한다.
    Revoke { device_id: String },
}

fn default_db() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME이 없습니다. --db로 경로를 주세요")?;
    Ok(PathBuf::from(home).join(".local/share/ai-task-notifier/notifier.db"))
}

#[tokio::main]
async fn main() -> Result<()> {
    // 훅은 인자 해석(clap)을 거치지 않는다: 해석 실패 시 종료 코드 2가 나오면
    // Claude Code가 그것을 '막기'로 받아들인다(Stop 중단 방지·프롬프트 삭제·도구 차단).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hook") {
        let provider = args.get(2).cloned().unwrap_or_default();
        // PermissionRequest 훅이 폰 답을 기다리는 최대 초(설정의 훅 timeout보다 짧게)
        let wait_secs = args
            .iter()
            .position(|a| a == "--wait-secs")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(claude_answer::DEFAULT_WAIT_SECS);
        let db = std::env::var_os("NOTIFIER_DB")
            .map(PathBuf::from)
            .or_else(|| default_db().ok());
        let mut input = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut input).is_err() {
            return Ok(());
        }
        let Some(db) = db else { return Ok(()) };
        if provider != "claude" {
            eprintln!("notifier hook: 지원하지 않는 훅 제공자: {provider}");
            return Ok(());
        }
        let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
        if v.get("hook_event_name").and_then(|e| e.as_str()) == Some("PermissionRequest") {
            // 폰 답을 기다린다. 표준 출력에는 훅 결정 JSON 하나만 쓴다(없으면 맥 화면의 답을 따름).
            match run_permission_hook(&db, &v, Duration::from_secs(wait_secs)).await {
                Ok(Some(out)) => {
                    // 표준 출력이 닫혀 있어도 패닉하지 않는다(종료 코드는 항상 0)
                    use std::io::Write;
                    let mut so = std::io::stdout().lock();
                    let _ = writeln!(so, "{out}").and_then(|_| so.flush());
                }
                Ok(None) => {}
                Err(e) => eprintln!("notifier hook: {e:#}"),
            }
            return Ok(());
        }
        match tokio::time::timeout(Duration::from_secs(4), run_hook(&db, &input)).await {
            Ok(Err(e)) => eprintln!("notifier hook: {e:#}"),
            Err(_) => eprintln!("notifier hook: 시간 초과(4초)"),
            Ok(Ok(())) => {}
        }
        return Ok(());
    }
    // 로그는 표준 오류로(훅의 표준 출력은 Claude 대화 문맥에 들어갈 수 있음)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => default_db()?,
    };

    let pool = db::open(&db_path).await?;
    let clock = Arc::new(SystemClock);
    // Claude Code 답 전달: PermissionRequest 훅이 기다리는 우편함에 넣는다
    let mut base = Service::new(pool.clone(), clock.clone()).with_provider(
        collect::CLAUDE,
        Arc::new(claude_answer::ClaudeHookProvider::new(pool.clone(), clock)),
    );
    if let Cmd::Serve {
        demo_provider: Some(name),
        ..
    } = &cli.cmd
    {
        tracing::warn!(
            "시험용 가짜 제공자 '{name}': 이 제공자의 답은 실제로 전달되지 않고 바로 수락됩니다"
        );
        base = base.with_provider(name, Arc::new(FakeProvider::new()));
    }
    let svc = Arc::new(base);

    match cli.cmd {
        Cmd::Pair { ttl_min } => {
            if !(1..=60).contains(&ttl_min) {
                bail!("--ttl-min은 1~60이어야 합니다");
            }
            let code = svc.create_pairing_code(ttl_min * 60_000).await?;
            println!("등록 코드: {code}  ({ttl_min}분 안에 폰에서 한 번만 쓸 수 있습니다)");
        }
        Cmd::Ingest => {
            use std::io::BufRead;
            let (mut ok, mut failed) = (0, 0);
            for (i, line) in std::io::stdin().lock().lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let result = match serde_json::from_str::<ProviderEvent>(&line) {
                    Ok(ev) => svc.ingest(&ev).await.map_err(|e| e.to_string()),
                    Err(e) => Err(format!("JSON 해석 실패: {e}")),
                };
                match result {
                    Ok(r) => {
                        ok += 1;
                        if let Some(id) = r.new_decision {
                            println!("{}: 새 결정 {id}", i + 1);
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}번째 줄: {e}", i + 1);
                    }
                }
            }
            println!("반영 {ok}건, 실패 {failed}건");
            if failed > 0 {
                bail!("일부 이벤트를 넣지 못했습니다");
            }
        }
        Cmd::Devices => {
            for d in svc.list_devices().await? {
                println!(
                    "{}  {}  등록 {}  마지막 {}  {}{}",
                    d.id,
                    d.name,
                    d.created_at
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M"),
                    d.last_seen_at
                        .map(|t| t
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string())
                        .unwrap_or_else(|| "-".into()),
                    if d.revoked { "해제됨" } else { "사용 중" },
                    if d.has_push { " · 푸시" } else { "" },
                );
            }
        }
        Cmd::Revoke { device_id } => {
            if svc.revoke_device(&device_id).await? {
                println!("해제했습니다: {device_id}");
            } else {
                bail!("사용 중인 기기가 아닙니다: {device_id}");
            }
        }
        Cmd::Hook { .. } => unreachable!("hook은 main 앞부분에서 처리"),
        Cmd::Serve {
            listen,
            allowed_origins,
            insecure_cookie,
            demo_provider: _,
            claude_usage,
            claude_bin,
            codex,
        } => {
            if !listen.ip().is_loopback() {
                bail!("루프백 주소에서만 실행합니다(폰 접속은 Tailscale Serve 등 사설망 HTTPS로)");
            }
            // 같은 DB로 서버가 둘 뜨면 재시작 복구가 다른 쪽의 전달 중인 답을 건드린다 → 하나만 허용
            let mut lock_path = db_path.as_os_str().to_owned();
            lock_path.push(".serve.lock");
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .with_context(|| format!("잠금 파일을 열 수 없습니다: {}", lock_path.display()))?;
            if lock.try_lock().is_err() {
                bail!("같은 DB로 이미 다른 notifier serve가 실행 중입니다");
            }
            serve(
                svc,
                listen,
                ApiConfig {
                    allowed_origins,
                    secure_cookie: !insecure_cookie,
                },
                Collectors {
                    claude_usage: claude_usage.map(|dir| ClaudeUsage {
                        bin: claude_bin,
                        config_dir: dir,
                        work_dir: db_path.with_file_name("claude-usage-cwd"),
                    }),
                    codex_home: codex,
                },
            )
            .await?;
            drop(lock);
        }
    }
    pool.close().await;
    Ok(())
}

struct ClaudeUsage {
    bin: PathBuf,
    config_dir: PathBuf,
    work_dir: PathBuf,
}

struct Collectors {
    claude_usage: Option<ClaudeUsage>,
    codex_home: Option<PathBuf>,
}

async fn run_hook(db_path: &std::path::Path, input: &str) -> Result<()> {
    let Some(pool) = db::open_for_hook(db_path).await? else {
        return Ok(()); // DB가 아직 없음(서버를 한 번도 안 띄움): 아무것도 하지 않는다
    };
    let svc = Service::new(pool.clone(), Arc::new(SystemClock));
    let r = collect::run_claude_hook(&svc, input).await;
    pool.close().await;
    r
}

async fn run_permission_hook(
    db_path: &std::path::Path,
    input: &serde_json::Value,
    wait: Duration,
) -> Result<Option<serde_json::Value>> {
    let Some(pool) = db::open_for_hook(db_path).await? else {
        return Ok(None);
    };
    let svc = Service::new(pool.clone(), Arc::new(SystemClock));
    let r = claude_answer::run_permission_hook(&svc, input, wait).await;
    pool.close().await;
    r
}

async fn serve(
    svc: Arc<Service>,
    listen: SocketAddr,
    cfg: ApiConfig,
    collectors: Collectors,
) -> Result<()> {
    let recovered = svc.recover_after_restart().await?;
    if recovered > 0 {
        tracing::warn!("재시작 전 전달 중이던 답 {recovered}건을 '반영 확인 중'으로 돌렸습니다");
    }
    let wake = Arc::new(Notify::new());
    let mut tasks = vec![tokio::spawn(run_worker(svc.clone(), wake.clone()))];
    if let Some(cu) = collectors.claude_usage {
        tracing::info!(
            "Claude 사용량 수집: 5분마다 /usage(설정 폴더 {})",
            cu.config_dir.display()
        );
        tasks.push(tokio::spawn(run_claude_usage(svc.clone(), cu)));
    }
    if let Some(home) = collectors.codex_home {
        tracing::info!("Codex 기록 감시: {}", home.join("sessions").display());
        tasks.push(tokio::spawn(run_codex_watch(svc.clone(), home)));
    }
    tracing::info!("Claude Code 답 전달: PermissionRequest 훅 우편함(Codex는 관측만)");

    let app = api::router(AppState {
        svc,
        cfg: Arc::new(cfg),
        wake,
    });
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("{listen}에 바인딩 실패"))?;
    tracing::info!("http://{listen} 에서 대기");
    // 종료 신호가 오면 새 연결을 받지 않고, 열린 스트림(폰)이 끊길 때까지 최대 3초만 기다린다
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
    });
    tokio::signal::ctrl_c()
        .await
        .context("종료 신호 대기 실패")?;
    tracing::info!("종료 중…");
    let _ = stop_tx.send(());
    if tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .is_err()
    {
        tracing::warn!("열린 연결을 기다리지 않고 종료합니다");
    }
    tasks.iter().for_each(|t| t.abort());
    Ok(())
}

/// Claude 사용량: 시작 직후 한 번, 이후 5분마다.
async fn run_claude_usage(svc: Arc<Service>, cu: ClaudeUsage) {
    loop {
        match collect::read_claude_usage(&cu.bin, &cu.config_dir, &cu.work_dir).await {
            Ok(windows) => {
                let ev = ProviderEvent {
                    provider: collect::CLAUDE.into(),
                    session_id: "-".into(),
                    event_id: format!("claude-usage:{}", uuid::Uuid::new_v4().simple()),
                    kind: ai_task_notifier::model::EventKind::UsageReported { windows },
                };
                if let Err(e) = svc.ingest(&ev).await {
                    tracing::warn!("Claude 사용량 반영 실패: {e}");
                }
            }
            Err(e) => tracing::warn!("Claude 사용량 읽기 실패: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

/// Codex 기록: 5초마다 새 줄 확인.
async fn run_codex_watch(svc: Arc<Service>, home: PathBuf) {
    let mut w = collect::CodexWatcher::new(&home);
    loop {
        if let Err(e) = w.poll(&svc).await {
            tracing::warn!("Codex 기록 읽기 실패: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// 마감 처리·답 전달(1초마다 또는 답 접수 즉시), 반영 확인(10초마다).
async fn run_worker(svc: Arc<Service>, wake: Arc<Notify>) {
    let mut last_reconcile = tokio::time::Instant::now();
    let mut last_sweep = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        if let Err(e) = svc.expire_due().await {
            tracing::error!("마감 처리 실패: {e:#}");
        }
        if let Err(e) = svc.deliver_pending().await {
            tracing::error!("답 전달 실패: {e:#}");
        }
        if last_sweep.elapsed() >= Duration::from_secs(60) {
            last_sweep = tokio::time::Instant::now();
            if let Err(e) = svc.sweep_sessions().await {
                tracing::error!("세션 정리 실패: {e:#}");
            }
            if let Err(e) = svc.purge_old_records().await {
                tracing::error!("지난 기록 정리 실패: {e:#}");
            }
            // 사라진 훅이 남긴 기다림·닫다 끊긴 결정 정리
            if let Err(e) = claude_answer::sweep_stale_waits(&svc).await {
                tracing::error!("훅 기다림 정리 실패: {e:#}");
            }
        }
        if last_reconcile.elapsed() >= Duration::from_secs(10) {
            last_reconcile = tokio::time::Instant::now();
            if let Err(e) = svc.reconcile_confirming().await {
                tracing::error!("반영 확인 실패: {e:#}");
            }
        }
    }
}
