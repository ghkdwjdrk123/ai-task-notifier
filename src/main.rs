//! notifier 명령: 서비스 실행, 기기 등록 코드 발급, 기기 목록·해제.

use ai_task_notifier::api::{self, ApiConfig, AppState};
use ai_task_notifier::clock::SystemClock;
use ai_task_notifier::db;
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
        /// 쓰기 요청에 허용할 Origin. 여러 번 줄 수 있다.
        #[arg(long = "allowed-origin", required = true)]
        allowed_origins: Vec<String>,
        /// 쿠키에서 Secure를 뺀다(HTTPS 없이 로컬에서 시험할 때만).
        #[arg(long)]
        insecure_cookie: bool,
    },
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
    tracing_subscriber::fmt()
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
    let svc = Arc::new(Service::new(pool.clone(), Arc::new(SystemClock)));

    match cli.cmd {
        Cmd::Pair { ttl_min } => {
            if !(1..=60).contains(&ttl_min) {
                bail!("--ttl-min은 1~60이어야 합니다");
            }
            let code = svc.create_pairing_code(ttl_min * 60_000).await?;
            println!("등록 코드: {code}  ({ttl_min}분 안에 폰에서 한 번만 쓸 수 있습니다)");
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
        Cmd::Serve {
            listen,
            allowed_origins,
            insecure_cookie,
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
            )
            .await?;
            drop(lock);
        }
    }
    pool.close().await;
    Ok(())
}

async fn serve(svc: Arc<Service>, listen: SocketAddr, cfg: ApiConfig) -> Result<()> {
    let recovered = svc.recover_after_restart().await?;
    if recovered > 0 {
        tracing::warn!("재시작 전 전달 중이던 답 {recovered}건을 '반영 확인 중'으로 돌렸습니다");
    }
    let wake = Arc::new(Notify::new());
    let worker = tokio::spawn(run_worker(svc.clone(), wake.clone()));
    // 1단계에는 실제 제공자·푸시 발송기가 없다(2단계에서 연결). 대기열은 DB에 쌓인다.
    tracing::info!(
        "제공자·푸시 발송기 미연결(1단계): 답은 전달되지 않고 푸시는 대기열에만 기록됩니다"
    );

    let app = api::router(AppState {
        svc,
        cfg: Arc::new(cfg),
        wake,
    });
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("{listen}에 바인딩 실패"))?;
    tracing::info!("http://{listen} 에서 대기");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    worker.abort();
    Ok(())
}

/// 마감 처리·답 전달(1초마다 또는 답 접수 즉시), 반영 확인(10초마다).
async fn run_worker(svc: Arc<Service>, wake: Arc<Notify>) {
    let mut last_reconcile = tokio::time::Instant::now();
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
        if last_reconcile.elapsed() >= Duration::from_secs(10) {
            last_reconcile = tokio::time::Instant::now();
            if let Err(e) = svc.reconcile_confirming().await {
                tracing::error!("반영 확인 실패: {e:#}");
            }
        }
    }
}
