use std::io::Read;

use azazel_remote::Access;

use crate::common::*;

const GZIP: (&str, &str) = ("Accept-Encoding", "gzip");

#[tokio::test]
async fn the_page_is_public_and_comes_gzipped_only_to_a_browser_that_accepts_it() {
    let (server, ..) = server().await;
    let port = server.port();
    let plain = request(port, "GET", "/", &[], "").await;
    assert_eq!(plain.status, 200);
    assert!(plain.text().contains("<html") && !plain.headers.contains_key("content-encoding"));
    let csp = &plain.headers["content-security-policy"];
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(plain.headers["x-content-type-options"], "nosniff");
    assert_eq!(plain.headers["referrer-policy"], "no-referrer");

    let zipped = request(port, "GET", "/", &[GZIP], "").await;
    assert_eq!(zipped.headers["content-encoding"], "gzip");
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&zipped.body[..])
        .read_to_string(&mut inflated)
        .unwrap();
    assert_eq!(inflated, plain.text());

    // A path the page routes by itself gets the page; a file that is not there does not.
    assert_eq!(request(port, "GET", "/s/abc", &[], "").await.text(), plain.text());
    assert_eq!(
        request(port, "GET", "/_app/immutable/gone.js", &[], "")
            .await
            .status,
        404
    );
    let tag = &plain.headers["etag"];
    assert_eq!(
        request(port, "GET", "/", &[("If-None-Match", tag)], "")
            .await
            .status,
        304
    );
}

#[tokio::test]
async fn a_request_for_another_host_or_origin_is_refused() {
    let (server, ..) = server().await;
    let port = server.port();
    let token = pair(&server, Access::Control).await;
    let auth = format!("Bearer {token}");
    let rebound = request(port, "GET", "/", &[("Host", "evil.example:80")], "").await;
    assert_eq!(rebound.status, 421);
    let rebound = request(
        port,
        "GET",
        "/api/me",
        &[("Host", "evil.example"), ("Authorization", &auth)],
        "",
    )
    .await;
    assert_eq!(rebound.status, 421);
    let foreign = request(
        port,
        "GET",
        "/api/me",
        &[("Origin", "https://evil.example"), ("Authorization", &auth)],
        "",
    )
    .await;
    assert_eq!(foreign.status, 403);
    let own = format!("http://localhost:{port}");
    let host = format!("localhost:{port}");
    let local = request(
        port,
        "GET",
        "/api/me",
        &[("Host", &host), ("Origin", &own), ("Authorization", &auth)],
        "",
    )
    .await;
    assert_eq!(local.status, 200);
}

#[tokio::test]
async fn the_api_needs_a_device_and_a_link_makes_one_once() {
    let (server, ..) = server().await;
    let port = server.port();
    for path in ["/api/me", "/api/sessions"] {
        assert_eq!(request(port, "GET", path, &[], "").await.status, 401, "{path}");
    }
    let nope = request(port, "GET", "/api/me", &[("Authorization", "Bearer nope")], "").await;
    assert_eq!(nope.status, 401);

    let link = server.link(Access::View);
    assert!(link.url.starts_with(&format!("http://127.0.0.1:{port}/#pair=")));
    let body = serde_json::json!({ "secret": secret(&link.url), "bearer": true }).to_string();
    // A POST that a plain HTML form could send is not accepted.
    assert_eq!(request(port, "POST", "/api/pair", &[], &body).await.status, 403);
    let first = request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &body).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.json()["access"], "view");
    let second = request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &body).await;
    assert_eq!(second.status, 401);

    let auth = format!("Bearer {}", first.json()["token"].as_str().unwrap());
    let me = request(port, "GET", "/api/me", &[("Authorization", &auth)], "").await;
    assert_eq!((me.status, me.json()["access"].clone()), (200, "view".into()));
    assert_eq!(me.headers["cache-control"], "no-store");
    let sessions = request(port, "GET", "/api/sessions", &[("Authorization", &auth)], "")
        .await
        .json();
    assert_eq!(sessions["sessions"][0]["id"], "s1");
    assert_eq!(sessions["sessions"][0]["kind"], "background");
}

#[tokio::test]
async fn a_browser_gets_a_cookie_and_signs_out_with_it() {
    let (server, ..) = server().await;
    let port = server.port();
    let link = server.link(Access::Control);
    let body = serde_json::json!({ "secret": secret(&link.url) }).to_string();
    let paired = request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &body).await;
    assert_eq!(paired.status, 200);
    assert!(paired.json().get("token").is_none());
    let cookie = &paired.headers["set-cookie"];
    assert!(
        cookie.starts_with("azazel_device=")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict")
    );
    assert!(!cookie.contains("Secure"), "plain http on localhost");
    let value = cookie.split(';').next().unwrap().to_string();

    assert_eq!(
        request(port, "GET", "/api/me", &[("Cookie", &value)], "")
            .await
            .status,
        200
    );
    // A request that rides on a cookie says where it comes from.
    let post = &[("Cookie", value.as_str()), ("X-Azazel", "1")];
    assert_eq!(request(port, "POST", "/api/logout", post, "").await.status, 403);
    let origin = format!("http://127.0.0.1:{port}");
    let with_origin = &[
        ("Cookie", value.as_str()),
        ("X-Azazel", "1"),
        ("Origin", origin.as_str()),
    ];
    assert_eq!(
        request(port, "POST", "/api/logout", with_origin, "").await.status,
        204
    );
    assert_eq!(
        request(port, "GET", "/api/me", &[("Cookie", &value)], "")
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn a_view_device_cannot_make_a_control_link() {
    let (server, ..) = server().await;
    let port = server.port();
    let view = format!("Bearer {}", pair(&server, Access::View).await);
    let control = format!("Bearer {}", pair(&server, Access::Control).await);
    let post = |auth: String, access: &'static str| async move {
        let body = format!(r#"{{"access":"{access}"}}"#);
        request(
            port,
            "POST",
            "/api/links",
            &[("Authorization", &auth), ("X-Azazel", "1")],
            &body,
        )
        .await
    };
    assert_eq!(post(view.clone(), "control").await.status, 403);
    assert_eq!(post(view, "view").await.status, 200);
    let made = post(control, "control").await;
    assert_eq!(made.status, 200);
    let url = made.json()["url"].as_str().unwrap().to_string();
    assert!(url.contains("#pair="));
    // The made link pairs, once.
    let body = serde_json::json!({ "secret": secret(&url), "bearer": true }).to_string();
    assert_eq!(
        request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &body)
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn too_many_wrong_secrets_are_turned_away_even_for_the_right_one() {
    let (server, ..) = server().await;
    let port = server.port();
    let link = server.link(Access::Control);
    let wrong = r#"{"secret":"nope"}"#;
    for _ in 0..5 {
        assert_eq!(
            request(port, "POST", "/api/pair", &[("X-Azazel", "1")], wrong)
                .await
                .status,
            401
        );
    }
    let right = serde_json::json!({ "secret": secret(&link.url) }).to_string();
    assert_eq!(
        request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &right)
            .await
            .status,
        429
    );
}

#[tokio::test]
async fn a_public_address_is_accepted_and_makes_the_cookie_secure() {
    let (server, ..) = server().await;
    let port = server.port();
    assert_eq!(
        request(port, "GET", "/", &[("Host", "abc.example.com")], "")
            .await
            .status,
        421
    );
    server.set_public_url("https://abc.example.com/").unwrap();
    assert_eq!(
        request(port, "GET", "/", &[("Host", "abc.example.com")], "")
            .await
            .status,
        200
    );
    let link = server.link(Access::Control);
    assert!(link.url.starts_with("https://abc.example.com/#pair="));
    let body = serde_json::json!({ "secret": secret(&link.url) }).to_string();
    let headers = [
        ("X-Azazel", "1"),
        ("Host", "abc.example.com"),
        ("Origin", "https://abc.example.com"),
    ];
    let paired = request(port, "POST", "/api/pair", &headers, &body).await;
    assert!(paired.headers["set-cookie"].contains("; Secure"));
    let other = [("X-Azazel", "1"), ("Origin", "https://other.example.com")];
    assert_eq!(
        request(port, "POST", "/api/pair", &other, &body).await.status,
        403
    );
    assert!(server.set_public_url("ftp://x").is_err());
}

#[tokio::test]
async fn a_taken_port_gives_way_to_the_next_one() {
    // Tests running beside this one hold ports of their own, so the port taken here is one whose
    // successor is free.
    let (_taken, port) = loop {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = taken.local_addr().unwrap().port();
        if port < u16::MAX - 10
            && tokio::net::TcpListener::bind(("127.0.0.1", port + 1))
                .await
                .is_ok()
        {
            break (taken, port);
        }
    };
    let (fake, _) = Fake::new();
    let mut options = options();
    options.port = port;
    let server = azazel_remote::Server::start(options, fake).await.unwrap();
    assert!((port + 1..port + 10).contains(&server.port()));
    assert!(server.urls()[0].ends_with(&server.port().to_string()));
}

#[tokio::test]
async fn a_controller_starts_a_session_in_a_folder_it_may_use() {
    let (server, fake, _) = server().await;
    let port = server.port();
    let control = format!("Bearer {}", pair(&server, Access::Control).await);
    let view = format!("Bearer {}", pair(&server, Access::View).await);
    // A browser reads what it may do from the pairing answer first.
    let link = server.link(Access::Control);
    let body = serde_json::json!({ "secret": secret(&link.url), "bearer": true }).to_string();
    let paired = request(port, "POST", "/api/pair", &[("X-Azazel", "1")], &body).await;
    assert_eq!(paired.json()["can_start"], true);
    let me =
        |auth: String| async move { request(port, "GET", "/api/me", &[("Authorization", &auth)], "").await };
    assert_eq!(me(control.clone()).await.json()["can_start"], true);
    assert_eq!(me(view.clone()).await.json()["can_start"], false);

    let post = |auth: String, path: &'static str, body: &'static str| async move {
        request(
            port,
            "POST",
            path,
            &[("Authorization", &auth), ("X-Azazel", "1")],
            body,
        )
        .await
    };
    let listed = post(control.clone(), "/api/folders", "{}").await;
    assert_eq!(listed.status, 200, "{}", listed.text());
    assert_eq!(listed.json()["folders"][0]["path"], "/work");
    let inside = post(control.clone(), "/api/folders", r#"{"under":"/work"}"#).await;
    assert_eq!(inside.json()["folders"][0]["path"], "/work/app");
    assert_eq!(
        post(control.clone(), "/api/folders", r#"{"under":"/etc"}"#)
            .await
            .status,
        403
    );

    let started = post(
        control.clone(),
        "/api/sessions",
        r#"{"cwd":"/work","prompt":"hi","mode":"auto","request_id":"r1"}"#,
    )
    .await;
    assert_eq!(started.status, 200, "{}", started.text());
    assert_eq!(started.json()["id"], "s2");
    let asked = fake.started.lock().unwrap()[0].clone();
    assert_eq!(
        (
            asked.prompt.as_deref(),
            asked.mode.as_deref(),
            asked.request_id.as_deref()
        ),
        (Some("hi"), Some("auto"), Some("r1"))
    );
    assert_eq!(
        post(control.clone(), "/api/sessions", r#"{"cwd":"/missing"}"#)
            .await
            .status,
        400
    );
    assert_eq!(
        post(control.clone(), "/api/sessions", r#"{"cwd":"/etc"}"#)
            .await
            .status,
        403
    );
    assert_eq!(
        post(control.clone(), "/api/sessions", r#"{"cwd":"  "}"#)
            .await
            .status,
        400
    );

    // A viewer can neither look nor start, and a page without the header cannot either.
    assert_eq!(post(view.clone(), "/api/folders", "{}").await.status, 403);
    assert_eq!(
        post(view, "/api/sessions", r#"{"cwd":"/work"}"#).await.status,
        403
    );
    let bare = request(
        port,
        "POST",
        "/api/sessions",
        &[("Authorization", &control)],
        r#"{"cwd":"/work"}"#,
    )
    .await;
    assert_eq!(bare.status, 403);
    assert_eq!(fake.started.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn bypass_is_refused_to_a_new_session_unless_allowed_and_starts_are_limited() {
    let (server, fake, _) = server().await;
    let port = server.port();
    let control = format!("Bearer {}", pair(&server, Access::Control).await);
    let post = |body: &'static str| {
        let control = control.clone();
        async move {
            request(
                port,
                "POST",
                "/api/sessions",
                &[("Authorization", &control), ("X-Azazel", "1")],
                body,
            )
            .await
        }
    };
    assert_eq!(post(r#"{"cwd":"/work","mode":"bypass"}"#).await.status, 403);
    assert!(fake.started.lock().unwrap().is_empty());
    for _ in 0..10 {
        assert_eq!(post(r#"{"cwd":"/work"}"#).await.status, 200);
    }
    let over = post(r#"{"cwd":"/work"}"#).await;
    assert_eq!(over.status, 429);
    assert_eq!(over.headers["retry-after"], "60");
    assert_eq!(fake.started.lock().unwrap().len(), 10);
}
