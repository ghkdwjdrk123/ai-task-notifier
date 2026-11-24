//! HTTP API 시험: 인증·출처 검사·응답 형식.

mod common;

use ai_task_notifier::api::{self, ApiConfig, AppState, COOKIE};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Notify;
use tower::ServiceExt;

const ORIGIN: &str = "https://mac.example.ts.net";

fn app(h: &H) -> Router {
    api::router(AppState {
        svc: h.svc.clone(),
        cfg: Arc::new(ApiConfig {
            allowed_origins: vec![ORIGIN.into()],
            secure_cookie: true,
        }),
        wake: Arc::new(Notify::new()),
    })
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, format!("other=1; {COOKIE}={c}"));
    }
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, v)
}

async fn paired(h: &H, app: &Router) -> String {
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let (st, headers, _) = call(
        app,
        "POST",
        "/v1/pairing/exchange",
        None,
        Some(ORIGIN),
        Some(json!({"code": code, "name": "갤럭시"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let set = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        set.contains("HttpOnly") && set.contains("Secure") && set.contains("SameSite=Lax"),
        "{set}"
    );
    set.split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = harness().await;
    let app = app(&h);
    let (st, headers, _) = call(&app, "GET", "/v1/snapshot", None, None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let (st, _, _) = call(
        &app,
        "GET",
        "/v1/snapshot",
        Some(&"0".repeat(64)),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, _) = call(&app, "GET", "/healthz", None, None, None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn t01_pairing_over_http_and_origin_check() {
    let h = harness().await;
    let app = app(&h);
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let body = json!({"code": code, "name": "갤럭시"});
    // Origin 없거나 다른 출처면 거절
    let (st, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        None,
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("bad_origin"))
    );
    let (st, _, _) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        Some("https://evil.example"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // 정상 등록 후 같은 코드 재사용 거절
    let (st, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        Some(ORIGIN),
        Some(body.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["name"], "갤럭시");
    let (st, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        Some(ORIGIN),
        Some(body),
    )
    .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("invalid_code"))
    );
}

#[tokio::test]
async fn full_answer_flow_over_http() {
    let h = harness().await;
    let app = app(&h);
    let token = paired(&h, &app).await;
    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q("어떻게 할까요?", &["중단", "건너뛰기"])],
            None,
        )
        .await;

    let (st, _, snap) = call(&app, "GET", "/v1/snapshot", Some(&token), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        (snap["running"].as_i64(), snap["waiting_decisions"].as_i64()),
        (Some(0), Some(1))
    );

    let (st, _, d) = call(
        &app,
        "GET",
        &format!("/v1/decisions/{id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(d["questions"][0]["options"][1]["label"], "건너뛰기");
    assert_eq!(d["answerable"], true);
    let version = d["version"].as_i64().unwrap();

    let body = json!({"submission_id": "sub-http-001", "decision_version": version, "answers": {"어떻게 할까요?": "건너뛰기"}});
    let uri = format!("/v1/decisions/{id}/answers");
    // 쓰기 요청은 Origin이 있어야 한다
    let (st, _, _) = call(&app, "POST", &uri, Some(&token), None, Some(body.clone())).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _, s) = call(
        &app,
        "POST",
        &uri,
        Some(&token),
        Some(ORIGIN),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (st, s["state"].as_str()),
        (StatusCode::ACCEPTED, Some("queued"))
    );
    // 같은 제출 재전송은 200과 기존 결과
    let (st, _, s2) = call(&app, "POST", &uri, Some(&token), Some(ORIGIN), Some(body)).await;
    assert_eq!(
        (st, s2["id"].as_str()),
        (StatusCode::OK, Some("sub-http-001"))
    );

    h.svc.deliver_pending().await.unwrap();
    let (st, _, s3) = call(
        &app,
        "GET",
        &format!("{uri}/sub-http-001"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(
        (st, s3["state"].as_str()),
        (StatusCode::OK, Some("applied"))
    );
    // 닫힌 요청에 새 제출은 409
    let late = json!({"submission_id": "sub-http-002", "decision_version": version, "answers": {"어떻게 할까요?": "중단"}});
    let (st, _, e) = call(&app, "POST", &uri, Some(&token), Some(ORIGIN), Some(late)).await;
    assert_eq!(
        (st, e["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_open"))
    );
    // 없는 결정은 404
    let (st, _, _) = call(
        &app,
        "GET",
        "/v1/decisions/no-such-id",
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn push_subscription_and_revocation() {
    let h = harness().await;
    let app = app(&h);
    let token = paired(&h, &app).await;
    let sub = serde_json::to_value(subscription("phone")).unwrap();
    let (st, _, _) = call(
        &app,
        "PUT",
        "/v1/device/push-subscription",
        Some(&token),
        Some(ORIGIN),
        Some(sub),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let bad =
        json!({"endpoint": "http://insecure.example/x", "keys": {"p256dh": "a", "auth": "b"}});
    let (st, _, _) = call(
        &app,
        "PUT",
        "/v1/device/push-subscription",
        Some(&token),
        Some(ORIGIN),
        Some(bad),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let dev = h.svc.list_devices().await.unwrap().remove(0);
    assert!(dev.has_push);
    // 맥에서 해제하면 같은 쿠키로 더는 못 들어온다(T02)
    h.svc.revoke_device(&dev.id).await.unwrap();
    let (st, _, _) = call(&app, "GET", "/v1/snapshot", Some(&token), None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}
