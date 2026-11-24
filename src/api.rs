//! 폰(PWA)용 HTTP API.
//!
//! 보안
//! - 서버는 루프백에서만 듣고, 폰은 Tailscale Serve 같은 사설망 HTTPS로 들어온다.
//! - 인증은 기기 쿠키(HttpOnly, SameSite=Lax). 토큰 원문은 DB에 없다.
//! - 쓰기 요청(POST·PUT·DELETE)은 Origin이 허용 목록에 있어야 한다(교차 사이트 요청 차단).
//! - 모든 응답은 `Cache-Control: no-store`.

use crate::push::PushSubscription;
use crate::service::{DeviceView, Service, ServiceError, SubmitRequest};
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Notify;

pub const COOKIE: &str = "atn_device";
const COOKIE_MAX_AGE_SECS: i64 = 90 * 24 * 3600;

#[derive(Debug, Clone, Default)]
pub struct ApiConfig {
    /// 쓰기 요청에 허용할 Origin(예: "https://mac.example.ts.net").
    pub allowed_origins: Vec<String>,
    /// 쿠키에 Secure를 붙인다(HTTPS로만 접속할 때 true).
    pub secure_cookie: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub svc: Arc<Service>,
    pub cfg: Arc<ApiConfig>,
    /// 답이 접수되면 전달 작업자를 깨운다.
    pub wake: Arc<Notify>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/pairing/exchange", post(pairing_exchange))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/decisions/{id}", get(decision_detail))
        .route("/v1/decisions/{id}/answers", post(submit_answer))
        .route("/v1/decisions/{id}/answers/{sid}", get(get_submission))
        .route(
            "/v1/device/push-subscription",
            put(put_push_subscription).delete(delete_push_subscription),
        )
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
    let write = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if write {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        let ok = origin.is_some_and(|o| st.cfg.allowed_origins.iter().any(|a| a == o));
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

impl FromRequestParts<AppState> for AuthDevice {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let token = cookie_value(&parts.headers, COOKIE).ok_or_else(ApiError::unauthorized)?;
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
}

#[derive(Serialize)]
struct PairResponse {
    device: DeviceView,
}

async fn pairing_exchange(
    State(st): State<AppState>,
    Json(req): Json<PairRequest>,
) -> Result<Response, ApiError> {
    let (device, token) = st.svc.exchange_pairing_code(&req.code, &req.name).await?;
    let secure = if st.cfg.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}; HttpOnly; SameSite=Lax{secure}"
    );
    let mut res = Json(PairResponse { device }).into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|e| ApiError::from(anyhow::Error::from(e)))?,
    );
    Ok(res)
}

async fn snapshot(State(st): State<AppState>, _d: AuthDevice) -> Result<Response, ApiError> {
    Ok(Json(st.svc.snapshot().await?).into_response())
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

async fn put_push_subscription(
    State(st): State<AppState>,
    AuthDevice(dev): AuthDevice,
    Json(sub): Json<PushSubscription>,
) -> Result<StatusCode, ApiError> {
    st.svc.set_push_subscription(&dev.id, &sub).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_push_subscription(
    State(st): State<AppState>,
    AuthDevice(dev): AuthDevice,
) -> Result<StatusCode, ApiError> {
    st.svc.delete_push_subscription(&dev.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
