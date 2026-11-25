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
    // 쿠키를 실은 쓰기 요청에 Origin이 없으면 거절(브라우저 교차 사이트 요청 차단)
    let (st, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        Some(&"0".repeat(64)),
        None,
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("bad_origin"))
    );
    // 다른 출처도 거절
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

// ---------- Android 앱 경로 ----------

async fn call_bearer(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn app_pairs_without_origin_and_uses_bearer() {
    let h = harness().await;
    let app = app(&h);
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let body = json!({"code": code, "name": "S22", "token_in_body": true});
    let (st, _, v) = call(&app, "POST", "/v1/pairing/exchange", None, None, Some(body)).await;
    assert_eq!(st, StatusCode::OK);
    let token = v["token"]
        .as_str()
        .expect("앱 등록은 본문으로 토큰을 받는다")
        .to_string();
    assert_eq!(token.len(), 64);

    h.start(CLAUDE, "s1").await;
    let id = h
        .decide(
            "s1",
            "r1",
            vec![q("어떻게 할까요?", &["중단", "건너뛰기"])],
            None,
        )
        .await;
    let (st, snap) = call_bearer(&app, "GET", "/v1/snapshot", &token, None).await;
    assert_eq!(
        (st, snap["waiting_decisions"].as_i64()),
        (StatusCode::OK, Some(1))
    );
    // Bearer 쓰기 요청은 Origin 없이 된다
    let version = snap["decisions"][0]["version"].as_i64().unwrap();
    let answer = json!({"submission_id": "sub-app-0001", "decision_version": version,
                        "answers": {"어떻게 할까요?": "중단"}});
    let (st, s) = call_bearer(
        &app,
        "POST",
        &format!("/v1/decisions/{id}/answers"),
        &token,
        Some(answer),
    )
    .await;
    assert_eq!(
        (st, s["state"].as_str()),
        (StatusCode::ACCEPTED, Some("queued"))
    );
    // 틀린 토큰은 401
    let (st, _) = call_bearer(&app, "GET", "/v1/snapshot", &"f".repeat(64), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn browser_pairing_never_returns_token_in_body() {
    let h = harness().await;
    let app = app(&h);
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let body = json!({"code": code, "name": "브라우저", "token_in_body": true});
    let (st, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        Some(ORIGIN),
        Some(body),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        v.get("token").is_none(),
        "Origin이 있는 요청에는 본문 토큰 없음"
    );
}

#[tokio::test]
async fn usage_reported_appears_in_snapshot() {
    use ai_task_notifier::model::{EventKind, UsageWindow};
    let h = harness().await;
    let app = app(&h);
    let token = paired(&h, &app).await;
    let w = |name: &str, p: f64| UsageWindow {
        name: name.into(),
        used_percent: p,
        resets_at: Some(T0 + 3_600_000),
        status: Some("allowed".into()),
    };
    let v0 = h.svc.snapshot_version().await.unwrap();
    h.send(ev(
        CLAUDE,
        "-",
        EventKind::UsageReported {
            windows: vec![w("five_hour", 48.0), w("seven_day", 4.0)],
        },
    ))
    .await;
    let v1 = h.svc.snapshot_version().await.unwrap();
    assert!(v1 > v0);
    // 소수점 아래만 바뀌면 화면 버전을 올리지 않는다
    h.send(ev(
        CLAUDE,
        "-",
        EventKind::UsageReported {
            windows: vec![w("five_hour", 48.2)],
        },
    ))
    .await;
    assert_eq!(h.svc.snapshot_version().await.unwrap(), v1);
    let (_, _, snap) = call(&app, "GET", "/v1/snapshot", Some(&token), None, None).await;
    let usage = snap["usage"].as_array().unwrap();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0]["provider"], CLAUDE);
    assert_eq!(usage[0]["window"], "five_hour");
    assert_eq!(usage[0]["used_percent"].as_f64(), Some(48.2));
    assert!(
        snap["sessions"].as_array().unwrap().is_empty(),
        "사용량 보고는 세션을 만들지 않음"
    );
    // 범위 밖 값은 거절
    let bad = ev(
        CLAUDE,
        "-",
        EventKind::UsageReported {
            windows: vec![w("five_hour", 140.0)],
        },
    );
    assert!(h.svc.ingest(&bad).await.is_err());
}

#[tokio::test]
async fn stream_sends_snapshot_then_changes() {
    let h = harness().await;
    let app = app(&h);
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let (_, _, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        None,
        Some(json!({"code": code, "name": "S22", "token_in_body": true})),
    )
    .await;
    let token = v["token"].as_str().unwrap().to_string();
    let req = Request::builder()
        .uri("/v1/stream")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let mut body = res.into_body();
    let mut text = String::new();
    // 첫 스냅샷
    while !text.contains("\n\n") {
        let frame = body.frame().await.unwrap().unwrap();
        if let Ok(d) = frame.into_data() {
            text.push_str(std::str::from_utf8(&d).unwrap());
        }
    }
    assert!(text.starts_with("event: snapshot"), "{text}");
    assert!(text.contains("\"running\":0"));
    // 상태가 바뀌면 다음 스냅샷이 온다
    h.start(CLAUDE, "s1").await;
    let mut next = String::new();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !next.contains("\"running\":1") {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(d) = frame.into_data() {
                next.push_str(std::str::from_utf8(&d).unwrap());
            }
        }
    })
    .await;
    assert!(got.is_ok(), "변경 후 5초 안에 새 스냅샷: {next}");
    // 기기를 해제하면 스트림이 닫힌다
    let dev = h.svc.list_devices().await.unwrap().remove(0);
    h.svc.revoke_device(&dev.id).await.unwrap();
    h.start(CLAUDE, "s2").await;
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match body.frame().await {
                None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "해제 후 스트림 종료");
}

#[tokio::test]
async fn app_pairing_sets_no_cookie_and_can_revoke_itself() {
    let h = harness().await;
    let app = app(&h);
    let code = h.svc.create_pairing_code(600_000).await.unwrap();
    let (st, headers, v) = call(
        &app,
        "POST",
        "/v1/pairing/exchange",
        None,
        None,
        Some(json!({"code": code, "name": "S22", "token_in_body": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        headers.get(header::SET_COOKIE).is_none(),
        "앱 등록에는 쿠키를 굽지 않음"
    );
    let token = v["token"].as_str().unwrap().to_string();
    // 앱의 '등록 해제' → 서버에서도 토큰 무효
    let (st, _) = call_bearer(&app, "DELETE", "/v1/device", &token, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call_bearer(&app, "GET", "/v1/snapshot", &token, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // 해제된 토큰으로는 스트림도 못 연다
    let (st, _) = call_bearer(&app, "GET", "/v1/stream", &token, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_usage_reports_are_rejected() {
    use ai_task_notifier::model::{EventKind, UsageWindow};
    let h = harness().await;
    let w = |name: &str, p: f64, status: Option<&str>| UsageWindow {
        name: name.into(),
        used_percent: p,
        resets_at: None,
        status: status.map(str::to_string),
    };
    let cases = vec![
        vec![w("five_hour", f64::NAN, None)],
        vec![w("five_hour", -1.0, None)],
        vec![w("five hour", 10.0, None)],
        vec![w("five_hour", 10.0, Some(&"x".repeat(41)))],
        (0..9).map(|i| w(&format!("w{i}"), 1.0, None)).collect(),
        vec![],
    ];
    for (i, windows) in cases.into_iter().enumerate() {
        let r = h
            .svc
            .ingest(&ev(CLAUDE, "-", EventKind::UsageReported { windows }))
            .await;
        assert!(r.is_err(), "경우 {i}는 거절돼야 함");
    }
    assert!(h.svc.snapshot().await.unwrap().usage.is_empty());
}
