//! 폰(Android 앱·브라우저)용 HTTP API.
//!
//! 보안
//! - 서버는 루프백에서만 듣고, 폰은 Tailscale Serve 같은 사설망 HTTPS로 들어온다.
//! - 인증: Android 앱은 `Authorization: Bearer <토큰>`, 브라우저는 기기 쿠키(HttpOnly, SameSite=Lax).
//!   토큰 원문은 DB에 없다.
//! - 쓰기 요청(POST·PUT·DELETE): Origin이 있으면 허용 목록에 있어야 한다. Origin이 없는 쓰기는
//!   쿠키를 싣지 않은 경우(앱의 Bearer 요청·등록)만 받는다 — 브라우저는 쓰기 요청에 항상 Origin을
//!   붙이므로 쿠키를 이용한 교차 사이트 요청은 막힌다.
//! - 모든 응답은 `Cache-Control: no-store`.

use crate::service::{DeviceView, Service, ServiceError, SubmitRequest};
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

pub const COOKIE: &str = "atn_device";
const COOKIE_MAX_AGE_SECS: i64 = 90 * 24 * 3600;

#[derive(Debug, Clone, Default)]
pub struct ApiConfig {
    /// 쓰기 요청에 허용할 Origin(예: "https://mac.example.ts.net").
    pub allowed_origins: Vec<String>,
    /// 쿠키에 Secure를 붙인다(HTTPS로만 접속할 때 true).
    pub secure_cookie: bool,
    /// 앱 업데이트 폴더(`latest.apk`·`latest.json`). 등록된 기기에만 내려준다.
    pub app_dir: Option<std::path::PathBuf>,
    /// 받아 줄 Host 이름(포트 제외). 루프백과 `*.ts.net`은 항상 받는다(DNS 리바인딩 방어).
    pub allowed_hosts: Vec<String>,
}

/// 기기 하나가 동시에 열 수 있는 실시간 스트림 수(재연결이 겹치는 정도만 허용).
const MAX_STREAMS_PER_DEVICE: usize = 4;

/// 기기별 열린 스트림 수.
pub type StreamCounts = Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>;

/// 스트림 자리 하나. 스트림이 끝나면(연결 끊김 포함) 돌려준다.
struct StreamSlot {
    counts: StreamCounts,
    device: String,
}

impl StreamSlot {
    fn take(counts: &StreamCounts, device: &str) -> Option<Self> {
        let mut m = counts.lock().unwrap();
        let n = m.entry(device.to_string()).or_insert(0);
        if *n >= MAX_STREAMS_PER_DEVICE {
            return None;
        }
        *n += 1;
        Some(Self {
            counts: counts.clone(),
            device: device.to_string(),
        })
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut m = self.counts.lock().unwrap();
        if let Some(n) = m.get_mut(&self.device) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.device);
            }
        }
    }
}

/// Host 헤더 검사: 포트를 떼고 루프백·`*.ts.net`·허용 목록만. Host가 없으면(브라우저 아님) 통과.
fn host_allowed(host: Option<&str>, extra: &[String]) -> bool {
    let Some(h) = host else { return true };
    let h = h.trim().to_ascii_lowercase();
    let name = if let Some(rest) = h.strip_prefix('[') {
        // [::1]:8770
        rest.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map_or(h.as_str(), |(n, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) { n } else { h.as_str() }
        })
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
        || (name.ends_with(".ts.net") && name.len() > ".ts.net".len())
        || extra.iter().any(|e| e.eq_ignore_ascii_case(name))
}

#[derive(Clone)]
pub struct AppState {
    pub svc: Arc<Service>,
    pub cfg: Arc<ApiConfig>,
    /// 답이 접수되면 전달 작업자를 깨운다.
    pub wake: Arc<Notify>,
    /// 기기별 열린 실시간 스트림 수.
    pub streams: StreamCounts,
    /// Claude 사용량 즉시 다시 읽기(수집기가 없으면 None).
    pub usage_refresh: Option<Arc<Notify>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/pairing/exchange", post(pairing_exchange))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/stream", get(stream))
        .route("/v1/decisions/{id}", get(decision_detail))
        .route("/v1/decisions/{id}/answers", post(submit_answer))
        .route("/v1/decisions/{id}/answers/{sid}", get(get_submission))
        .route("/v1/sessions/{id}/ack", post(ack_session))
        .route("/v1/usage/refresh", post(refresh_usage))
        .route("/v1/device", delete(revoke_self))
        .route("/v1/app/latest", get(app_latest))
        .route("/v1/app/apk", get(app_apk))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

// ---------- 오류 ----------

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "기기 등록이 필요합니다",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "찾을 수 없습니다")
            }
            ServiceError::Conflict {
                code: "invalid_code",
                message,
            } => Self::new(StatusCode::FORBIDDEN, "invalid_code", message),
            ServiceError::Conflict { code, message } => {
                Self::new(StatusCode::CONFLICT, code, message)
            }
            ServiceError::Invalid(m) => Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m),
            ServiceError::Internal(e) => e.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // 내부 오류 내용(질문·경로 등)은 응답에 넣지 않고 로그에만 남긴다
        tracing::error!("내부 오류: {e:#}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "서버 오류가 발생했습니다",
        )
    }
}

// ---------- 공통 검사 ----------

async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    // DNS 리바인딩: 남의 도메인 이름으로 들어온 요청은 처음부터 받지 않는다
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
    if !host_allowed(host, &st.cfg.allowed_hosts) {
        tracing::warn!("허용되지 않은 Host로 온 요청 거절: {:?}", host);
        let mut res = ApiError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "bad_host",
            "허용되지 않은 주소로 온 요청입니다",
        )
        .into_response();
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return res;
    }
    let write = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if write {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        let has_cookie = req.headers().contains_key(header::COOKIE);
        let ok = match origin {
            Some(o) => st.cfg.allowed_origins.iter().any(|a| a == o),
            // 브라우저가 아닌 클라이언트(앱): 쿠키 인증이 아니면 Origin 없이 허용
            None => !has_cookie,
        };
        if !ok {
            let mut res = ApiError::new(
                StatusCode::FORBIDDEN,
                "bad_origin",
                "허용되지 않은 출처의 요청입니다",
            )
            .into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return res;
        }
    }
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    res
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// 인증된 기기.
pub struct AuthDevice(pub DeviceView);

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

/// 요청의 기기 토큰: Bearer가 있으면 그것, 없으면 쿠키.
fn device_token(headers: &HeaderMap) -> Option<&str> {
    bearer_token(headers).or_else(|| cookie_value(headers, COOKIE))
}

impl FromRequestParts<AppState> for AuthDevice {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let token = device_token(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        st.svc
            .authenticate(token)
            .await?
            .map(AuthDevice)
            .ok_or_else(ApiError::unauthorized)
    }
}

// ---------- 처리기 ----------

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: String,
    /// 앱이 토큰을 직접 받아 보관할 때 true. 브라우저(Origin 있음)에는 주지 않는다.
    #[serde(default)]
    token_in_body: bool,
}

#[derive(Serialize)]
struct PairResponse {
    device: DeviceView,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

async fn pairing_exchange(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PairRequest>,
) -> Result<Response, ApiError> {
    let (device, token) = st.svc.exchange_pairing_code(&req.code, &req.name).await?;
    // 브라우저 스크립트가 토큰을 읽지 못하게: Origin이 있는 요청에는 본문으로 주지 않는다
    let body_token =
        (req.token_in_body && !headers.contains_key(header::ORIGIN)).then(|| token.clone());
    let secure = if st.cfg.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}; HttpOnly; SameSite=Lax{secure}"
    );
    let app_client = body_token.is_some();
    let mut res = Json(PairResponse {
        device,
        token: body_token,
    })
    .into_response();
    // 앱은 쿠키를 쓰지 않으므로 토큰을 본문으로 준 경우에는 쿠키를 굽지 않는다
    if !app_client {
        res.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).map_err(|e| ApiError::from(anyhow::Error::from(e)))?,
        );
    }
    Ok(res)
}

async fn snapshot(State(st): State<AppState>, _d: AuthDevice) -> Result<Response, ApiError> {
    Ok(Json(st.svc.snapshot().await?).into_response())
}

/// 실시간 스냅샷 스트림(SSE). 버전이 바뀌면 바로, 아니어도 30초마다 `snapshot` 이벤트를 보낸다.
/// 앱은 이 연결 하나로 상시 알림을 갱신한다(푸시 없음). 기기가 해제되면 스트림을 닫는다.
async fn stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    AuthDevice(dev): AuthDevice,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let Some(slot) = StreamSlot::take(&st.streams, &dev.id) else {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_streams",
            "이 기기의 실시간 연결이 너무 많습니다. 잠시 뒤 다시 연결하세요",
        ));
    };
    let token = device_token(&headers).unwrap_or_default().to_string();
    let svc = st.svc.clone();
    let events = futures_util::stream::unfold(
        (svc, token, None::<i64>, Instant::now(), slot),
        |(svc, token, last, mut sent_at, slot)| async move {
            if last.is_some() {
                // 다음 보낼 때까지 1초 간격으로 버전 확인
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let v = svc.snapshot_version().await.ok();
                    if v != last || sent_at.elapsed() >= Duration::from_secs(30) {
                        break;
                    }
                }
            }
            match svc.authenticate(&token).await {
                Ok(Some(_)) => {}
                Ok(None) => return None, // 해제된 기기: 스트림 종료
                Err(e) => tracing::warn!("스트림 재인증 일시 실패(계속 진행): {e:#}"),
            }
            let (event, version) = match svc.snapshot().await {
                Ok(snap) => {
                    let v = snap.version;
                    let data = serde_json::to_string(&snap).unwrap_or_default();
                    (Event::default().event("snapshot").data(data), Some(v))
                }
                Err(e) => {
                    tracing::error!("스트림 스냅샷 실패: {e:#}");
                    // 실패가 이어져도 1초마다 DB를 두드리지 않게 잠시 쉰다
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    (
                        Event::default().event("error").data("snapshot_failed"),
                        last,
                    )
                }
            };
            sent_at = Instant::now();
            Some((Ok(event), (svc, token, version.or(Some(-1)), sent_at, slot)))
        },
    );
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn decision_detail(
    State(st): State<AppState>,
    _d: AuthDevice,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(st.svc.decision_detail(&id).await?).into_response())
}

async fn submit_answer(
    State(st): State<AppState>,
    AuthDevice(dev): AuthDevice,
    Path(id): Path<String>,
    Json(req): Json<SubmitRequest>,
) -> Result<Response, ApiError> {
    let (sub, created) = st.svc.submit(Some(&dev.id), &id, &req).await?;
    if created {
        st.wake.notify_one();
    }
    let status = if created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(sub)).into_response())
}

async fn get_submission(
    State(st): State<AppState>,
    _d: AuthDevice,
    Path((id, sid)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Ok(Json(st.svc.get_submission(&id, &sid).await?).into_response())
}

/// 폰의 '사용량 새로고침': Claude 사용량을 바로 다시 읽게 한다(결과는 실시간 스트림으로).
/// 수집기는 직전 읽기에서 20초가 지나지 않았으면 그때까지 미룬다.
async fn refresh_usage(State(st): State<AppState>, _d: AuthDevice) -> Result<StatusCode, ApiError> {
    let Some(n) = &st.usage_refresh else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_usage_collector",
            "맥에서 Claude 사용량 수집이 꺼져 있습니다",
        ));
    };
    n.notify_one();
    Ok(StatusCode::ACCEPTED)
}

/// 지시 대기 세션 '확인함'(지시 대기 수에서 뺌).
async fn ack_session(
    State(st): State<AppState>,
    _d: AuthDevice,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    st.svc.acknowledge_session(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 앱의 '이 폰 등록 해제': 서버에서도 이 기기의 토큰을 무효로 한다.
async fn revoke_self(
    State(st): State<AppState>,
    AuthDevice(dev): AuthDevice,
) -> Result<StatusCode, ApiError> {
    st.svc.revoke_device(&dev.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 앱 업데이트 ----------

/// 받을 APK 크기 상한(요청마다 메모리로 읽으므로).
const MAX_APK_BYTES: u64 = 200 * 1024 * 1024;

/// `latest.json`(android/publish.sh가 씀): 버전과 APK의 크기·SHA-256.
#[derive(Deserialize, Serialize)]
struct AppLatest {
    version_code: i64,
    version_name: String,
    size: u64,
    /// APK 전체의 SHA-256(소문자 16진). 앱이 받은 파일이 온전한지 확인한다.
    sha256: String,
}

fn no_app() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "no_app",
        "맥에 올려 둔 앱 업데이트가 없습니다",
    )
}

fn publishing() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "publishing",
        "앱 업데이트를 올리는 중입니다. 잠시 뒤 다시 시도하세요",
    )
}

/// 앱 업데이트 폴더의 버전 정보. 폴더·파일이 없으면 no_app.
async fn app_meta(st: &AppState) -> Result<(std::path::PathBuf, AppLatest), ApiError> {
    let dir = st.cfg.app_dir.clone().ok_or_else(no_app)?;
    let raw = match tokio::fs::read(dir.join("latest.json")).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(no_app()),
        Err(e) => return Err(anyhow::Error::from(e).into()),
    };
    let meta: AppLatest = serde_json::from_slice(&raw).map_err(anyhow::Error::from)?;
    match tokio::fs::metadata(dir.join("latest.apk")).await {
        Ok(m) if m.len() == meta.size && m.len() <= MAX_APK_BYTES => Ok((dir, meta)),
        Ok(_) => Err(publishing()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(no_app()),
        Err(e) => Err(anyhow::Error::from(e).into()),
    }
}

async fn app_latest(State(st): State<AppState>, _d: AuthDevice) -> Result<Response, ApiError> {
    let (_, meta) = app_meta(&st).await?;
    Ok(Json(meta).into_response())
}

async fn app_apk(State(st): State<AppState>, _d: AuthDevice) -> Result<Response, ApiError> {
    let (dir, meta) = app_meta(&st).await?;
    let apk = tokio::fs::read(dir.join("latest.apk"))
        .await
        .map_err(anyhow::Error::from)?;
    use sha2::Digest;
    // 올리는 도중(APK와 버전 파일이 어긋남)이면 내려주지 않는다
    if apk.len() as u64 != meta.size || hex::encode(sha2::Sha256::digest(&apk)) != meta.sha256 {
        return Err(publishing());
    }
    let mut res = apk.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.android.package-archive"),
    );
    if let Ok(v) = HeaderValue::from_str(&meta.sha256) {
        h.insert("x-apk-sha256", v);
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::host_allowed;

    #[test]
    fn host_allowlist() {
        let extra = vec!["mac.lan".to_string()];
        for ok in [
            None,
            Some("127.0.0.1:8770"),
            Some("localhost:8770"),
            Some("[::1]:8770"),
            Some("mac-mini.example.ts.net:8443"),
            Some("Mac-Mini.Example.TS.NET"),
            Some("mac.lan:8770"),
        ] {
            assert!(host_allowed(ok, &extra), "{ok:?}");
        }
        for bad in [
            Some("evil.example:8770"),
            Some("127.0.0.1.evil.example"),
            Some("ts.net"),
            Some(".ts.net"),
            Some("evilts.net"),
            Some("192.168.0.10:8770"),
        ] {
            assert!(!host_allowed(bad, &extra), "{bad:?}");
        }
    }
}
