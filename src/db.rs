//! SQLite 연결과 스키마 마이그레이션.
//!
//! 이 서비스의 쓰기량은 작으므로 연결 1개짜리 풀로 모든 접근을 직렬화한다.
//! 쓰기 트랜잭션은 `BEGIN IMMEDIATE`로 시작해 CLI 같은 다른 프로세스와 겹칠 때도
//! 읽기→쓰기 승격 충돌(SQLITE_BUSY)을 피한다.
//! **주의**: 트랜잭션을 쥔 채 같은 풀에서 다시 연결을 얻으면 교착된다.
//! 트랜잭션 안의 도우미 함수는 반드시 `&mut SqliteConnection`을 받는다.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::path::Path;
use std::time::Duration;

const V1_INIT: &str = include_str!("../migrations/0001_init.sql");
const V2_USAGE: &str = include_str!("../migrations/0002_usage.sql");
pub const SCHEMA_VERSION: i64 = 2;

pub type Tx = Transaction<'static, Sqlite>;

/// DB 파일을 열고(없으면 만들고) 마이그레이션한다. 파일 권한은 600으로 맞춘다.
pub async fn open(file: &Path) -> Result<SqlitePool> {
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty())
        && !dir.exists()
    {
        std::fs::create_dir_all(dir).context("DB 폴더를 만들 수 없습니다")?;
        restrict(dir, 0o700)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(file)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10))
        // 지운 질문·답변이 빈 페이지에 남지 않게 0으로 덮어쓴다
        .pragma("secure_delete", "on");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("DB를 열 수 없습니다: {}", file.display()))?;
    restrict(file, 0o600)?;
    migrate(&pool).await?;
    // WAL 보조 파일에도 질문·답변 평문이 잠시 남으므로 같은 권한으로 맞춘다
    for suffix in ["-wal", "-shm"] {
        let mut side = file.as_os_str().to_owned();
        side.push(suffix);
        let side = Path::new(&side);
        if side.exists() {
            restrict(side, 0o600)?;
        }
    }
    Ok(pool)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("권한을 바꿀 수 없습니다: {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// PRAGMA user_version으로 스키마 버전을 관리한다.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    // 이미 최신이면 쓰기 잠금 없이 끝낸다(훅처럼 자주 여는 경우의 비용·잠금 대기를 줄임)
    let current: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    // 버전은 쓰기 잠금을 잡은 뒤 다시 읽는다(다른 프로세스가 동시에 처음 열 때 두 번 적용하지 않게)
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version > SCHEMA_VERSION {
        anyhow::bail!("DB 스키마({version})가 이 프로그램({SCHEMA_VERSION})보다 새롭습니다");
    }
    if version < 1 {
        sqlx::raw_sql(V1_INIT)
            .execute(&mut *tx)
            .await
            .context("스키마 v1 적용 실패")?;
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut *tx)
            .await?;
    }
    if version < 2 {
        sqlx::raw_sql(V2_USAGE)
            .execute(&mut *tx)
            .await
            .context("스키마 v2 적용 실패")?;
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 훅용: 이미 있는 최신 DB만 연다(없으면 None — 만들지 않음). 잠금 대기는 3초로 짧게.
pub async fn open_for_hook(file: &Path) -> Result<Option<SqlitePool>> {
    if !file.exists() {
        return Ok(None);
    }
    let opts = SqliteConnectOptions::new()
        .filename(file)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(3))
        .pragma("secure_delete", "on");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let v: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await?;
    if v != SCHEMA_VERSION {
        // 서버가 아직 마이그레이션하지 않은 DB: 훅은 건드리지 않는다
        pool.close().await;
        return Ok(None);
    }
    Ok(Some(pool))
}

/// 쓰기 트랜잭션 시작.
pub async fn begin(pool: &SqlitePool) -> Result<Tx> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}
