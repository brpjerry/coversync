//! Pairing devices and managing them, through the whole app as it is served.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};

use super::*;

/// The server itself: a desktop app on it, or curl.
const LOCAL: &str = "127.0.0.1:50000";
const CA: &str = include_str!("../testdata/ca.pem");
/// `openssl x509 -noout -fingerprint -sha256 -in testdata/ca.pem`
const OPENSSL: &str = "26:43:AD:A5:72:9B:B0:FE:6C:D0:50:0D:65:BB:48:C9:12:02:37:66:0E:00:0B:0C:9D:84:6C:4C:7D:48:0C:D4";
/// Not a real key: only the armor matters.
const KEY: &str = "-----BEGIN PRIVATE KEY-----\nTm90IGEga2V5\n-----END PRIVATE KEY-----\n";

fn request(method: &str, path: &str, auth: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut req = Request::builder().method(method).uri(path);
    match auth {
        Some(t) if t.starts_with("cst_") => {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"))
        }
        Some(cookie) => req = req.header(header::COOKIE, cookie),
        None => {}
    }
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    req.body(body).unwrap()
}

impl App {
    /// A request signed in with `auth`: a cookie, or a `cst_` token.
    async fn call(&self, method: &str, path: &str, auth: &str, body: Option<Value>) -> Response {
        self.send(request(method, path, Some(auth), body)).await
    }

    /// Add device, as the browser does: the pairing's id and its code.
    async fn new_code(&self, cookie: &str, body: Value) -> (String, String) {
        let res = self.call("POST", "/api/pairings", cookie, Some(body)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        let b = super::body(res).await;
        (
            b["id"].as_str().unwrap().to_owned(),
            b["code"].as_str().unwrap().to_owned(),
        )
    }

    async fn pair_from(&self, peer: &str, req: Request<Body>) -> Response {
        self.send_from(peer, req).await
    }

    async fn pairing(&self, cookie: &str, id: &str) -> Value {
        let res = self
            .call("GET", &format!("/api/pairings/{id}"), cookie, None)
            .await;
        super::body(res).await
    }

    async fn devices(&self, auth: &str) -> Value {
        super::body(self.call("GET", "/api/devices", auth, None).await).await
    }

    async fn pairing_setup(
        &self,
        cookie: &str,
        ca_path: Option<&str>,
        allow_http: bool,
    ) -> Response {
        let body = json!({ "ca_path": ca_path, "allow_http": allow_http });
        self.call("PUT", "/api/pairing/setup", cookie, Some(body))
            .await
    }
}

fn pair(code: &str) -> Request<Body> {
    let body = json!({
        "code": code,
        "platform": "android",
        "app_version": "0.1.0",
        "hostname": "pixel-8",
    });
    request("POST", "/api/pair", None, Some(body))
}

/// Through a proxy on this machine, as Caddy sends it.
fn proxied(code: &str, scheme: &str, client: &str) -> Request<Body> {
    let mut req = pair(code);
    let h = req.headers_mut();
    h.insert("x-forwarded-proto", scheme.parse().unwrap());
    h.insert("x-forwarded-for", client.parse().unwrap());
    req
}

fn phone() -> Value {
    json!({ "name": "Pixel 8", "scope": "manage" })
}

/// "K7QM4ZTD" as Add device shows it: "K7QM-4ZTD", typed in lowercase.
fn typed(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..]).to_lowercase()
}

async fn paired(res: Response) -> (i64, String) {
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    let b = body(res).await;
    (
        b["device_id"].as_i64().unwrap(),
        b["token"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn pairing_by_typed_code_and_by_link() {
    let app = App::with_password("pair");
    let cookie = app.cookie().await;
    let res = app
        .call("POST", "/api/pairings", &cookie, Some(phone()))
        .await;
    let b = body(res).await;
    assert_eq!(b["expires_in"], 600);
    assert_eq!(
        (b["ca"].clone(), b["allow_http"].clone()),
        (Value::Null, json!(false))
    );
    let (id, code) = (b["id"].as_str().unwrap(), b["code"].as_str().unwrap());
    assert_eq!(code.len(), 8);
    assert_eq!(app.pairing(&cookie, id).await["state"], "waiting");

    // Typed on the desktop: grouped, in lowercase.
    let res = app.pair_from(LOCAL, pair(&typed(code))).await;
    let b = body(res).await;
    let device = b["device_id"].as_i64().unwrap();
    let token = b["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("cst_") && token.len() == 47, "{token}");
    assert_eq!(b["name"], "Pixel 8");
    assert_eq!(b["scope"], "manage");
    assert_eq!(b["server"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(b["server"]["name"].is_string());
    assert_eq!(b["library_id"], Value::Null);

    // The dialog shows who it was.
    let status = app.pairing(&cookie, id).await;
    assert_eq!(status["state"], "paired");
    let d = &status["device"];
    assert_eq!(d["id"], device);
    assert_eq!(
        (
            d["platform"].clone(),
            d["app_version"].clone(),
            d["hostname"].clone()
        ),
        (json!("android"), json!("0.1.0"), json!("pixel-8"))
    );
    assert_eq!(d["last_ip"], "127.0.0.1");

    // The token works.
    assert_eq!(
        app.send(bearer("/api/status", &token)).await.status(),
        StatusCode::OK
    );
    let res = app.send(bearer("/api/session", &token)).await;
    assert_eq!(
        body(res).await,
        json!({ "via": "token", "scope": "manage", "device": device })
    );

    // From the QR code's link: the code as it is there, through Caddy.
    let (_, code) = app
        .new_code(&cookie, json!({ "name": "Laptop", "scope": "manage" }))
        .await;
    let link =
        format!("coversync://pair?server=https%3A%2F%2Fn5pro.internal%2Fcoversync%2F&code={code}");
    let from_link = link.rsplit("code=").next().unwrap();
    let (laptop, token) = paired(
        app.pair_from(LOCAL, proxied(from_link, "https", "192.168.1.30"))
            .await,
    )
    .await;
    assert_ne!(laptop, device);
    assert_eq!(
        app.send(bearer("/api/albums", &token)).await.status(),
        StatusCode::OK
    );
    let list = app.devices(&cookie).await;
    let names: Vec<&str> = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Pixel 8", "Laptop"]);
    assert_eq!(list["devices"][1]["last_ip"], "192.168.1.30");
    assert_eq!(list["can_manage"], true);
    assert_eq!(list["this_device"], Value::Null);
}

#[tokio::test]
async fn used_expired_and_wrong_codes_are_refused() {
    let app = App::with_password("refused");
    let cookie = app.cookie().await;

    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let res = app.pair_from(LOCAL, pair(&code)).await;
    assert_eq!(code_of(res).await, (StatusCode::GONE, "code_used".into()));

    let res = app.pair_from(LOCAL, pair("00000000")).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::BAD_REQUEST, "wrong_code".into())
    );
    for bad in ["", "K7QM", "K7QM-4ZTU", "K7QM-4ZTD-X"] {
        let res = app.pair_from(LOCAL, pair(bad)).await;
        assert_eq!(
            code_of(res).await,
            (StatusCode::BAD_REQUEST, "bad_code".into()),
            "{bad:?}"
        );
    }

    // Ten minutes later.
    let (id, code) = app.new_code(&cookie, phone()).await;
    app.state
        .auth
        .advance_clock(Duration::from_secs(9 * 60 + 59));
    assert_eq!(app.pairing(&cookie, &id).await["expires_in"], 1);
    app.state.auth.advance_clock(Duration::from_secs(1));
    assert_eq!(app.pairing(&cookie, &id).await["state"], "expired");
    let res = app.pair_from(LOCAL, pair(&code)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::GONE, "code_expired".into())
    );

    // Closing the dialog cancels its code.
    let (id, code) = app
        .new_code(&cookie, json!({ "name": "Other", "scope": "sync" }))
        .await;
    let res = app
        .call("DELETE", &format!("/api/pairings/{id}"), &cookie, None)
        .await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = app.pair_from("127.0.0.2:1", pair(&code)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::BAD_REQUEST, "wrong_code".into())
    );
    let res = app
        .call("GET", &format!("/api/pairings/{id}"), &cookie, None)
        .await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::NOT_FOUND, "no_pairing".into())
    );

    // Nothing was added by any of it.
    assert_eq!(
        app.devices(&cookie).await["devices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

async fn code_of(res: Response) -> (StatusCode, String) {
    super::code(res).await
}

#[tokio::test]
async fn ten_failures_cancel_the_waiting_codes() {
    let app = App::with_password("failures");
    let cookie = app.cookie().await;
    let (a, code) = app.new_code(&cookie, phone()).await;
    let (b, _) = app
        .new_code(&cookie, json!({ "name": "Player", "scope": "sync" }))
        .await;
    // From many addresses, so the per-client limit doesn't stop them first.
    for i in 0..9 {
        let res = app
            .pair_from(&format!("127.0.1.{i}:1"), pair("00000000"))
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "attempt {i}");
    }
    assert_eq!(app.pairing(&cookie, &a).await["state"], "waiting");
    app.pair_from("127.0.1.9:1", pair("00000000")).await;
    for id in [&a, &b] {
        let status = app.pairing(&cookie, id).await;
        assert_eq!(status, json!({ "state": "cancelled", "why": "failures" }));
    }
    let res = app.pair_from("127.0.2.1:1", pair(&code)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::GONE, "code_expired".into())
    );
    // A new code works.
    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(app.pair_from("127.0.2.2:1", pair(&code)).await).await;
}

#[tokio::test]
async fn pairing_attempts_are_rate_limited_per_client() {
    let app = App::with_password("pair-limit");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let me = "127.0.0.5:1";
    for _ in 0..5 {
        let res = app.pair_from(me, pair("00000000")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
    // Even the right code waits now.
    let res = app.pair_from(me, pair(&code)).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()[header::RETRY_AFTER], "60");
    // Logins are counted apart.
    assert_eq!(
        app.login_from(me, PASSWORD).await.status(),
        StatusCode::NO_CONTENT
    );
    // Another client isn't held up, and the code still works.
    paired(app.pair_from("127.0.0.6:1", pair(&code)).await).await;
}

#[tokio::test]
async fn plain_http_is_refused_except_on_loopback_or_when_allowed() {
    let app = App::with_password("http");
    let cookie = app.cookie().await;

    // Straight from the LAN: coversyncd itself only speaks plain http.
    let (id, code) = app.new_code(&cookie, phone()).await;
    let res = app.pair_from(LAN, pair(&code)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::FORBIDDEN, "https_required".into())
    );
    // The code crossed the network in the clear, so it's cancelled.
    let status = app.pairing(&cookie, &id).await;
    assert_eq!(status, json!({ "state": "cancelled", "why": "insecure" }));
    let res = app
        .pair_from(LOCAL, proxied(&code, "https", "192.168.1.20"))
        .await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::GONE, "code_expired".into())
    );

    // Claiming HTTPS doesn't help from the LAN; only the proxy here is believed.
    let (_, code) = app.new_code(&cookie, phone()).await;
    let mut claim = pair(&code);
    claim
        .headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    let res = app.pair_from(LAN, claim).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // Through the proxy, but over plain http from the LAN.
    let (_, code) = app.new_code(&cookie, phone()).await;
    let res = app
        .pair_from(LOCAL, proxied(&code, "http", "192.168.1.20"))
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // Anything a proxy forwarded over plain http came over a network, even
    // naming this machine, a client it couldn't parse, or none at all.
    for (proto, client) in [
        (Some("http"), Some("127.0.0.1")),
        (Some("http"), Some("not an address")),
        (Some("http"), None),
        (None, Some("192.168.1.20")),
    ] {
        let (id, code) = app.new_code(&cookie, phone()).await;
        let mut req = pair(&code);
        if let Some(p) = proto {
            req.headers_mut()
                .insert("x-forwarded-proto", p.parse().unwrap());
        }
        if let Some(c) = client {
            req.headers_mut()
                .insert("x-forwarded-for", c.parse().unwrap());
        }
        let res = app.pair_from(LOCAL, req).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{proto:?} {client:?}");
        assert_eq!(app.pairing(&cookie, &id).await["why"], "insecure");
    }
    // Through the proxy over HTTPS: fine.
    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(
        app.pair_from(LOCAL, proxied(&code, "https", "192.168.1.20"))
            .await,
    )
    .await;
    // Straight from this machine itself, over plain http.
    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(app.pair_from("[::1]:5000", pair(&code)).await).await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(app.pair_from(LOCAL, pair(&code)).await).await;

    // A code sent in the clear is cancelled even when the rest of the
    // request is wrong.
    let (id, code) = app.new_code(&cookie, phone()).await;
    let malformed = json!({ "code": code, "platform": "x".repeat(33) });
    let res = app
        .pair_from(LAN, request("POST", "/api/pair", None, Some(malformed)))
        .await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::FORBIDDEN, "https_required".into())
    );
    assert_eq!(app.pairing(&cookie, &id).await["why"], "insecure");

    // With the override, for a network that's encrypted already.
    let res = app.pairing_setup(&cookie, None, true).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body(res).await["setup"]["allow_http"], true);
    let (_, code) = app.new_code(&cookie, phone()).await;
    paired(app.pair_from("100.64.0.7:5000", pair(&code)).await).await;
}

#[tokio::test]
async fn a_sync_only_token_gets_403_on_management_routes() {
    let app = App::with_password("sync-scope");
    let cookie = app.cookie().await;
    let (_, code) = app
        .new_code(&cookie, json!({ "name": "AP80", "scope": "sync" }))
        .await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    for (method, path) in [
        ("GET", "/api/status"),
        ("GET", "/api/albums"),
        ("GET", "/api/devices"),
        ("POST", "/api/pairings"),
        ("PUT", "/api/library/setup"),
        ("GET", "/api/tracks/1/stream"),
        ("PUT", "/api/pairing/setup"),
    ] {
        let res = app.call(method, path, &token, Some(json!({}))).await;
        assert_eq!(
            code_of(res).await,
            (StatusCode::FORBIDDEN, "sync_only".into()),
            "{method} {path}"
        );
    }
    // It can check its token, though.
    let res = app.send(bearer("/api/session", &token)).await;
    assert_eq!(
        body(res).await,
        json!({ "via": "token", "scope": "sync", "device": device })
    );
}

#[tokio::test]
async fn neither_a_token_nor_its_cookie_can_manage_devices() {
    let app = App::with_password("not-password");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let res = app.send(trade(Some(&token), Some("/"))).await;
    let traded = cookie_of(&res);
    let (pending, _) = app.new_code(&cookie, phone()).await;

    for auth in [token.as_str(), traded.as_str()] {
        let list = app.devices(auth).await;
        assert_eq!(list["can_manage"], false);
        assert_eq!(list["this_device"], device);
        for (method, path, body) in [
            ("POST", "/api/pairings".to_owned(), Some(phone())),
            (
                "PUT",
                format!("/api/devices/{device}"),
                Some(json!({ "name": "Mine now" })),
            ),
            (
                "PUT",
                format!("/api/devices/{device}"),
                Some(json!({ "scope": "sync" })),
            ),
            ("POST", format!("/api/devices/{device}/revoke"), None),
            ("DELETE", format!("/api/devices/{device}"), None),
            ("GET", format!("/api/pairings/{pending}"), None),
            ("DELETE", format!("/api/pairings/{pending}"), None),
            (
                "PUT",
                "/api/pairing/setup".to_owned(),
                Some(json!({ "ca_path": null, "allow_http": true })),
            ),
            (
                "GET",
                "/api/pairing/check-ca?path=/etc/hostname".to_owned(),
                None,
            ),
        ] {
            let res = app.call(method, &path, auth, body).await;
            assert_eq!(
                code_of(res).await,
                (StatusCode::FORBIDDEN, "password_required".into()),
                "{method} {path}"
            );
        }
    }
    // Nothing changed.
    let list = app.devices(&cookie).await;
    let d = &list["devices"][0];
    assert_eq!(
        (d["name"].clone(), d["scope"].clone()),
        (json!("Pixel 8"), json!("manage"))
    );
    assert_eq!(d["revoked_at"], Value::Null);
    assert_eq!(app.pairing(&cookie, &pending).await["state"], "waiting");
    assert_eq!(
        body(app.call("GET", "/api/pairing/setup", &cookie, None).await).await["setup"]["allow_http"],
        false
    );
}

#[tokio::test]
async fn revoking_locks_the_device_out_at_once() {
    let app = App::with_password("revoke");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let res = app.send(trade(Some(&token), Some("/"))).await;
    let traded = cookie_of(&res);
    assert_eq!(
        app.get("/api/status", Some(&traded)).await.status(),
        StatusCode::OK
    );

    let res = app
        .call(
            "POST",
            &format!("/api/devices/{device}/revoke"),
            &cookie,
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body(res).await["revoked_at"].is_i64());
    let res = app.send(bearer("/api/status", &token)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::UNAUTHORIZED, "invalid_token".into())
    );
    let res = app.get("/api/status", Some(&traded)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::UNAUTHORIZED, "session_expired".into())
    );
    // It's listed as revoked until removed.
    let list = app.devices(&cookie).await;
    assert!(list["devices"][0]["revoked_at"].is_i64());
    let res = app
        .call("DELETE", &format!("/api/devices/{device}"), &cookie, None)
        .await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(app.devices(&cookie).await["devices"], json!([]));
    for (method, path) in [
        ("DELETE", format!("/api/devices/{device}")),
        ("POST", format!("/api/devices/{device}/revoke")),
    ] {
        let res = app.call(method, &path, &cookie, None).await;
        assert_eq!(
            code_of(res).await,
            (StatusCode::NOT_FOUND, "no_device".into())
        );
    }
}

#[tokio::test]
async fn replacing_a_device_keeps_its_id() {
    let app = App::with_password("replace");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, old) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let created = app.devices(&cookie).await["devices"][0]["created_at"].clone();

    // Reinstalled: Replace an existing device.
    let (id, code) = app
        .new_code(
            &cookie,
            json!({ "name": "Pixel 8", "scope": "manage", "replace": device }),
        )
        .await;
    let (same, new) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    assert_eq!(same, device);
    assert_eq!(app.pairing(&cookie, &id).await["device"]["id"], device);
    let res = app.send(bearer("/api/status", &old)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        app.send(bearer("/api/status", &new)).await.status(),
        StatusCode::OK
    );
    let list = app.devices(&cookie).await;
    assert_eq!(list["devices"].as_array().unwrap().len(), 1);
    assert_eq!(list["devices"][0]["created_at"], created);

    // A revoked one comes back the same way, with the name and scope chosen.
    app.call(
        "POST",
        &format!("/api/devices/{device}/revoke"),
        &cookie,
        None,
    )
    .await;
    let (_, code) = app
        .new_code(
            &cookie,
            json!({ "name": "Pixel 9", "scope": "sync", "replace": device }),
        )
        .await;
    let (same, _) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    assert_eq!(same, device);
    let d = &app.devices(&cookie).await["devices"][0];
    assert_eq!(
        (
            d["name"].clone(),
            d["scope"].clone(),
            d["revoked_at"].clone()
        ),
        (json!("Pixel 9"), json!("sync"), Value::Null)
    );

    // There's nothing to replace.
    let res = app
        .call(
            "POST",
            "/api/pairings",
            &cookie,
            Some(json!({ "name": "X", "scope": "manage", "replace": 999 })),
        )
        .await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::NOT_FOUND, "no_device".into())
    );
    // Removing the device cancels a code that would have replaced it, and
    // the dialog says why.
    let (id, code) = app
        .new_code(
            &cookie,
            json!({ "name": "Pixel 8", "scope": "manage", "replace": device }),
        )
        .await;
    app.call("DELETE", &format!("/api/devices/{device}"), &cookie, None)
        .await;
    let status = app.pairing(&cookie, &id).await;
    assert_eq!(status, json!({ "state": "cancelled", "why": "removed" }));
    let res = app.pair_from(LOCAL, pair(&code)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::GONE, "code_expired".into())
    );
    assert_eq!(app.devices(&cookie).await["devices"], json!([]));
}

#[tokio::test]
async fn a_removed_devices_id_never_reaches_another() {
    let app = App::with_password("reuse");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (first, _) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (second, _) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    // The newest goes, and another pairs: a page still showing the removed
    // one must not change the new one through its id.
    app.call("DELETE", &format!("/api/devices/{second}"), &cookie, None)
        .await;
    let (_, code) = app
        .new_code(&cookie, json!({ "name": "Laptop", "scope": "manage" }))
        .await;
    let (third, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    assert!(third > second && second > first);
    for (method, path, body) in [
        (
            "PUT",
            format!("/api/devices/{second}"),
            Some(json!({ "name": "stale" })),
        ),
        ("POST", format!("/api/devices/{second}/revoke"), None),
        ("DELETE", format!("/api/devices/{second}"), None),
    ] {
        let res = app.call(method, &path, &cookie, body).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
    assert_eq!(
        app.send(bearer("/api/status", &token)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_cookie_traded_for_a_sync_token_follows_its_devices_scope() {
    let app = App::with_password("promote");
    let cookie = app.cookie().await;
    let (_, code) = app
        .new_code(&cookie, json!({ "name": "Tablet", "scope": "sync" }))
        .await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let res = app.send(trade(Some(&token), Some("/"))).await;
    let traded = cookie_of(&res);
    let res = app.get("/api/albums", Some(&traded)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::FORBIDDEN, "sync_only".into())
    );
    // Promoted: the cookie it already has may manage at once, and back.
    let path = format!("/api/devices/{device}");
    for (scope, status) in [("manage", StatusCode::OK), ("sync", StatusCode::FORBIDDEN)] {
        app.call("PUT", &path, &cookie, Some(json!({ "scope": scope })))
            .await;
        let res = app.get("/api/albums", Some(&traded)).await;
        assert_eq!(res.status(), status, "{scope}");
    }
}

#[tokio::test]
async fn a_token_replaced_can_no_longer_be_traded() {
    let app = App::with_password("trade-replace");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, old) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let (_, code) = app
        .new_code(
            &cookie,
            json!({ "name": "Pixel 8", "scope": "manage", "replace": device }),
        )
        .await;
    paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let res = app.send(trade(Some(&old), Some("/"))).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::UNAUTHORIZED, "invalid_token".into())
    );
    // The trade that checked the old token just before the replacement
    // committed is the store's: `create_device_session` is tried with it.
    assert!(
        app.state
            .auth
            .store()
            .create_device_session(&old, now())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn devices_can_be_renamed_and_rescoped() {
    let app = App::with_password("rename");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    let path = format!("/api/devices/{device}");
    let res = app
        .call(
            "PUT",
            &path,
            &cookie,
            Some(json!({ "name": "  Jerry's phone " })),
        )
        .await;
    assert_eq!(body(res).await["name"], "Jerry's phone");
    let res = app
        .call("PUT", &path, &cookie, Some(json!({ "scope": "sync" })))
        .await;
    let b = body(res).await;
    assert_eq!(
        (b["name"].clone(), b["scope"].clone()),
        (json!("Jerry's phone"), json!("sync"))
    );
    // A narrower scope applies at once.
    let res = app.send(bearer("/api/status", &token)).await;
    assert_eq!(
        code_of(res).await,
        (StatusCode::FORBIDDEN, "sync_only".into())
    );
    for name in [
        json!(""),
        json!("   "),
        json!("x".repeat(65)),
        json!("a\nb"),
    ] {
        let res = app
            .call("PUT", &path, &cookie, Some(json!({ "name": name })))
            .await;
        assert_eq!(
            code_of(res).await,
            (StatusCode::BAD_REQUEST, "bad_name".into()),
            "{name}"
        );
        let res = app
            .call(
                "POST",
                "/api/pairings",
                &cookie,
                Some(json!({ "name": name, "scope": "sync" })),
            )
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{name}");
    }
    let res = app
        .call(
            "PUT",
            "/api/devices/999",
            &cookie,
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("coversyncd-ca-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn the_ca_setting_refuses_private_keys_and_serves_the_certificate() {
    let app = App::with_password("ca");
    let cookie = app.cookie().await;
    let dir = scratch("setting");
    let (cert, key, both) = (
        dir.join("rootCA.pem"),
        dir.join("rootCA-key.pem"),
        dir.join("both.pem"),
    );
    std::fs::write(&cert, CA).unwrap();
    std::fs::write(&key, KEY).unwrap();
    std::fs::write(&both, format!("{CA}{KEY}")).unwrap();

    // None yet.
    let res = app.get("/api/hello/ca", None).await;
    assert_eq!(code_of(res).await, (StatusCode::NOT_FOUND, "no_ca".into()));

    for path in [&key, &both] {
        let res = app.pairing_setup(&cookie, path.to_str(), false).await;
        let status = res.status();
        let b = body(res).await;
        assert_eq!(
            (status, b["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("bad_ca"))
        );
        assert!(b["error"].as_str().unwrap().contains("private key"), "{b}");
        let res = app
            .call(
                "GET",
                &format!("/api/pairing/check-ca?path={}", path.display()),
                &cookie,
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    for bad in ["rootCA.pem", "/nowhere/rootCA.pem"] {
        let res = app.pairing_setup(&cookie, Some(bad), false).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let res = app.get("/api/hello/ca", None).await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "nothing refused was saved"
    );

    let res = app
        .call(
            "GET",
            &format!("/api/pairing/check-ca?path={}", cert.display()),
            &cookie,
            None,
        )
        .await;
    assert_eq!(body(res).await["sha256"], OPENSSL);
    let res = app.pairing_setup(&cookie, cert.to_str(), false).await;
    assert_eq!(res.status(), StatusCode::OK);
    let b = body(res).await;
    assert_eq!(b["ca"]["sha256"], OPENSSL);
    let hex = OPENSSL.replace(':', "").to_lowercase();
    assert_eq!(b["ca"]["fingerprint"], hex);
    assert_eq!(b["ca_problem"], Value::Null);

    // Served without a login, and its fingerprint goes in the QR code.
    let res = app.get("/api/hello/ca", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/x-pem-file"
    );
    let served = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(served, CA.as_bytes());
    let res = app
        .call("POST", "/api/pairings", &cookie, Some(phone()))
        .await;
    assert_eq!(body(res).await["ca"], hex);

    // A key put in its place later is never served.
    std::fs::write(&cert, format!("{CA}{KEY}")).unwrap();
    let res = app.get("/api/hello/ca", None).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let text = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&text).contains("PRIVATE"));
    let b = body(app.call("GET", "/api/pairing/setup", &cookie, None).await).await;
    assert!(b["ca_problem"].as_str().unwrap().contains("private key"));
    let b = body(
        app.call("POST", "/api/pairings", &cookie, Some(phone()))
            .await,
    )
    .await;
    assert_eq!(b["ca"], Value::Null);
    assert!(b["ca_problem"].is_string());

    // The rest of the setup can still be saved with that path unchanged;
    // the answer says what's wrong with it.
    let res = app.pairing_setup(&cookie, cert.to_str(), true).await;
    assert_eq!(res.status(), StatusCode::OK);
    let b = body(res).await;
    assert_eq!(b["setup"]["allow_http"], true);
    assert_eq!(b["setup"]["ca_path"], cert.to_str().unwrap());
    assert!(b["ca_problem"].as_str().unwrap().contains("private key"));
    assert_eq!(
        app.get("/api/hello/ca", None).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    // A new path is still checked.
    let res = app.pairing_setup(&cookie, both.to_str(), false).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Cleared: back to none.
    let res = app.pairing_setup(&cookie, Some(""), false).await;
    assert_eq!(body(res).await["setup"]["ca_path"], Value::Null);
}

#[tokio::test]
async fn using_a_token_notes_where_from_and_the_app_version() {
    let app = App::with_password("seen");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    let (device, token) = paired(app.pair_from(LOCAL, pair(&code)).await).await;
    app.state.auth.store().forget_last_seen(device).unwrap();
    let mut req = bearer("/api/status", &token);
    req.headers_mut()
        .insert("coversync-app-version", "0.2.0".parse().unwrap());
    assert_eq!(
        app.send_from("192.168.1.77:1000", req).await.status(),
        StatusCode::OK
    );
    // Written in the background.
    for _ in 0..250 {
        let d = app.state.auth.store().device_info(device).unwrap().unwrap();
        if d.last_seen_at.is_some() {
            assert_eq!(d.last_ip.as_deref(), Some("192.168.1.77"));
            assert_eq!(d.app_version.as_deref(), Some("0.2.0"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the token's use wasn't noted");
}

#[tokio::test]
async fn what_devices_say_about_themselves_is_checked() {
    let app = App::with_password("about");
    let cookie = app.cookie().await;
    let (_, code) = app.new_code(&cookie, phone()).await;
    for body in [
        json!({ "code": code, "platform": "x".repeat(33) }),
        json!({ "code": code, "app_version": "1\n2" }),
        json!({ "code": code, "hostname": "h".repeat(254) }),
    ] {
        let res = app
            .pair_from(LOCAL, request("POST", "/api/pair", None, Some(body)))
            .await;
        assert_eq!(
            code_of(res).await,
            (StatusCode::BAD_REQUEST, "bad_request".into())
        );
    }
    // None of that used the code; a bare one is fine.
    let res = app
        .pair_from(
            LOCAL,
            request("POST", "/api/pair", None, Some(json!({ "code": code }))),
        )
        .await;
    let (device, _) = paired(res).await;
    let d = app.state.auth.store().device_info(device).unwrap().unwrap();
    assert_eq!((d.platform, d.hostname, d.app_version), (None, None, None));
}
