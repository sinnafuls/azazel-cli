//! The routes: the page, the API, and the upgrade to a WebSocket.

use std::net::IpAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, ETAG,
    HOST, IF_NONE_MATCH, ORIGIN, RETRY_AFTER, SET_COOKIE, UPGRADE, VARY,
};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::assets;
use crate::auth::{Device, PairError};
use crate::server::{Shared, rfc3339};
use crate::{Access, NewSession, StartError, ws};

pub(crate) type Res = Response<Full<Bytes>>;

const BODY_LIMIT: usize = 64 * 1024;
pub(crate) const MESSAGE_LIMIT: usize = 12 * 1024 * 1024;
const COOKIE_NAME: &str = "azazel_device";
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; \
style-src 'self' 'unsafe-inline'; frame-ancestors 'none'";

fn value(text: &str) -> hyper::header::HeaderValue {
    hyper::header::HeaderValue::from_str(text).unwrap_or_else(|_| hyper::header::HeaderValue::from_static(""))
}

pub(crate) async fn handle(shared: Arc<Shared>, peer: IpAddr, req: Request<Incoming>) -> Res {
    let is_api = req.uri().path() == "/api" || req.uri().path().starts_with("/api/");
    let mut res = route(&shared, peer, req).await;
    let headers = res.headers_mut();
    headers.insert("x-content-type-options", value("nosniff"));
    headers.insert("referrer-policy", value("no-referrer"));
    headers.insert("content-security-policy", value(CSP));
    if is_api {
        headers.insert(CACHE_CONTROL, value("no-store"));
    }
    res
}

fn reply(status: u16, content_type: &str, body: impl Into<Bytes>) -> Res {
    let mut res = Response::new(Full::new(body.into()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(CONTENT_TYPE, value(content_type));
    res
}

fn json_reply(status: u16, body: Value) -> Res {
    reply(status, "application/json", body.to_string())
}

fn error(status: u16, message: &str) -> Res {
    json_reply(status, json!({ "error": message }))
}

fn header(req: &Request<Incoming>, name: impl hyper::header::AsHeaderName) -> Option<&str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

/// The device token a request carries, and whether it came in a cookie.
fn token(req: &Request<Incoming>) -> Option<(String, bool)> {
    if let Some(bearer) = header(req, "authorization").and_then(|v| v.strip_prefix("Bearer ")) {
        return Some((bearer.trim().to_string(), false));
    }
    let cookies = header(req, COOKIE)?;
    cookies.split(';').find_map(|part| {
        let (name, content) = part.trim().split_once('=')?;
        (name == COOKIE_NAME && !content.is_empty()).then(|| (content.to_string(), true))
    })
}

/// The device a request is signed in as, if any. A cookie is sent by the browser on its own, so a
/// request that rides on one must also say where it came from.
fn authenticate(
    shared: &Shared,
    req: &Request<Incoming>,
    needs_origin: bool,
) -> Result<Option<(String, Device)>, Box<Res>> {
    let Some((token, cookie)) = token(req) else {
        return Ok(None);
    };
    if cookie && needs_origin && header(req, ORIGIN).is_none() {
        return Err(Box::new(error(
            403,
            "a request signed in by cookie must say where it comes from (`Origin`)",
        )));
    }
    Ok(shared.auth.device(&token).map(|device| (token, device)))
}

fn signed_in(
    shared: &Shared,
    req: &Request<Incoming>,
    needs_origin: bool,
) -> Result<(String, Device), Box<Res>> {
    authenticate(shared, req, needs_origin)?.ok_or_else(|| {
        Box::new(error(
            401,
            "not signed in: open the pairing link again, or ask for a new one",
        ))
    })
}

async fn route(shared: &Arc<Shared>, peer: IpAddr, mut req: Request<Incoming>) -> Res {
    if !shared.host_ok(header(&req, HOST).unwrap_or_default()) {
        return error(421, "this server does not answer to that address");
    }
    let path = req.uri().path().to_string();
    if !(path == "/api" || path.starts_with("/api/")) {
        return page(&req);
    }
    if let Some(origin) = header(&req, ORIGIN)
        && !shared.origin_ok(origin)
    {
        return error(403, "that origin is not one of this server's pages");
    }
    if matches!(*req.method(), Method::POST | Method::DELETE) && header(&req, "x-azazel") != Some("1") {
        return error(403, "a POST or DELETE needs the header `X-Azazel: 1`");
    }
    match (req.method().clone(), path.as_str()) {
        (Method::POST, "/api/pair") => pair(shared, peer, req).await,
        (Method::GET, "/api/me") => match signed_in(shared, &req, false) {
            Ok((_, device)) => json_reply(
                200,
                json!({
                    "access": device.access,
                    "expires": rfc3339(device.expires),
                    "allow_bypass": shared.grants.allow_bypass || shared.grants.full_access,
                    "full_access": shared.grants.full_access,
                    "can_start": device.access == Access::Control && shared.sessions.can_start(),
                    "protocol": azazel_protocol::PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                }),
            ),
            Err(res) => *res,
        },
        (Method::GET, "/api/devices") => devices(shared, &req),
        (Method::DELETE, "/api/devices") => sign_out_all(shared, &req),
        (Method::GET, "/api/sessions") => match signed_in(shared, &req, false) {
            Ok(_) => {
                let mut sessions = shared.sessions.list().await;
                sessions.sort_by(|a, b| b.started.cmp(&a.started));
                json_reply(200, json!({ "sessions": sessions }))
            }
            Err(res) => *res,
        },
        (Method::POST, "/api/links") => links(shared, req).await,
        (Method::POST, "/api/folders") => folders(shared, req).await,
        (Method::POST, "/api/sessions") => start(shared, req).await,
        (Method::POST, "/api/logout") => match signed_in(shared, &req, true) {
            Ok((token, _)) => {
                shared.auth.sign_out(&token);
                let mut res = reply(204, "application/json", Bytes::new());
                res.headers_mut().insert(
                    SET_COOKIE,
                    value(&format!(
                        "{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
                    )),
                );
                res
            }
            Err(res) => *res,
        },
        (Method::GET, p) if p.starts_with("/api/ws/") => socket(shared, &mut req, &p["/api/ws/".len()..]),
        (
            _,
            "/api/pair" | "/api/me" | "/api/sessions" | "/api/links" | "/api/logout" | "/api/folders"
            | "/api/devices",
        ) => error(405, "that route does not take this method"),
        (_, p) if p.starts_with("/api/ws/") => error(405, "that route does not take this method"),
        _ => error(404, "no such route"),
    }
}

/// The web page, or the file of the build the path names.
fn page(req: &Request<Incoming>) -> Res {
    let head = req.method() == Method::HEAD;
    if !head && req.method() != Method::GET {
        return error(405, "the page is read with GET");
    }
    let Some(asset) = assets::find(req.uri().path()) else {
        return reply(404, "text/plain; charset=utf-8", "not found");
    };
    let etag = asset.etag;
    let mut res = if header(req, IF_NONE_MATCH) == Some(etag) {
        reply(304, asset.mime, Bytes::new())
    } else {
        let gzip = header(req, ACCEPT_ENCODING)
            .is_some_and(|accepted| accepted.split(',').any(|part| part.trim().starts_with("gzip")));
        let (body, encoded) = if gzip {
            (Bytes::from_static(asset.gz), true)
        } else {
            (Bytes::from(assets::inflate(asset)), false)
        };
        let length = body.len();
        let mut res = reply(200, asset.mime, if head { Bytes::new() } else { body });
        if encoded {
            res.headers_mut().insert(CONTENT_ENCODING, value("gzip"));
        }
        if head {
            res.headers_mut().insert(CONTENT_LENGTH, length.into());
        }
        res
    };
    let headers = res.headers_mut();
    headers.insert(VARY, value("Accept-Encoding"));
    headers.insert(CACHE_CONTROL, value(assets::cache_control(asset)));
    headers.insert(ETAG, value(etag));
    res
}

async fn body(req: Request<Incoming>) -> Result<Bytes, Box<Res>> {
    Limited::new(req.into_body(), BODY_LIMIT)
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|_| Box::new(error(413, "the request is too large")))
}

#[derive(Deserialize)]
struct PairBody {
    secret: String,
    #[serde(default)]
    bearer: bool,
}

async fn pair(shared: &Arc<Shared>, peer: IpAddr, req: Request<Incoming>) -> Res {
    let host = header(&req, HOST).unwrap_or_default().to_ascii_lowercase();
    let secure = shared.is_https_host(&host);
    let bytes = match body(req).await {
        Ok(bytes) => bytes,
        Err(res) => return *res,
    };
    let Ok(pair) = serde_json::from_slice::<PairBody>(&bytes) else {
        return error(400, "send {\"secret\": \"…\"}");
    };
    match shared.auth.pair(&pair.secret, peer) {
        Ok((token, device)) => {
            let seconds = device
                .expires
                .duration_since(shared.auth.now())
                .map(|left| left.as_secs())
                .unwrap_or_default();
            let mut answer = json!({
                "access": device.access,
                "expires": rfc3339(device.expires),
                "allow_bypass": shared.grants.allow_bypass || shared.grants.full_access,
                "full_access": shared.grants.full_access,
                "can_start": device.access == Access::Control && shared.sessions.can_start(),
            });
            if pair.bearer {
                answer["token"] = Value::String(token);
                return json_reply(200, answer);
            }
            let mut res = json_reply(200, answer);
            let secure = if secure { "; Secure" } else { "" };
            res.headers_mut().insert(
                SET_COOKIE,
                value(&format!(
                    "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={seconds}{secure}"
                )),
            );
            res
        }
        Err(PairError::Invalid) => error(
            401,
            "that link does not work: it was used, it expired, or it is wrong",
        ),
        Err(PairError::TooMany) => {
            let mut res = error(429, "too many attempts: wait a minute");
            res.headers_mut().insert(RETRY_AFTER, value("60"));
            res
        }
    }
}

#[derive(Deserialize)]
struct LinkBody {
    access: Access,
}

async fn links(shared: &Arc<Shared>, req: Request<Incoming>) -> Res {
    let device = match signed_in(shared, &req, true) {
        Ok((_, device)) => device,
        Err(res) => return *res,
    };
    let origin = header(&req, ORIGIN).map(str::to_string);
    let bytes = match body(req).await {
        Ok(bytes) => bytes,
        Err(res) => return *res,
    };
    let Ok(wanted) = serde_json::from_slice::<LinkBody>(&bytes) else {
        return error(400, "send {\"access\": \"view\"} or {\"access\": \"control\"}");
    };
    if wanted.access == Access::Control && device.access != Access::Control {
        return error(403, "a view device can make view links only");
    }
    // The page that asked is the page the link should open, whichever way it was reached.
    let origin = origin.unwrap_or_else(|| shared.origin());
    json_reply(
        200,
        serde_json::to_value(shared.link(wanted.access, &origin)).unwrap_or_default(),
    )
}

/// The paired devices, for a device with control access. Only a short label (the first characters of
/// the hash of the token) names one: no token or secret leaves the server.
fn devices(shared: &Shared, req: &Request<Incoming>) -> Res {
    let (_, me) = match signed_in(shared, req, false) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    if me.access != Access::Control {
        return error(403, "this is a view link: listing devices needs a control link");
    }
    let rows: Vec<Value> = shared
        .auth
        .devices()
        .iter()
        .map(|device| {
            json!({
                "id": device.id,
                "access": device.access,
                "paired": rfc3339(device.paired),
                "expires": rfc3339(device.expires),
                "current": device.id == me.id,
            })
        })
        .collect();
    json_reply(200, json!({ "devices": rows }))
}

/// Signs every device out and voids every link, as `r` does in the server's terminal.
fn sign_out_all(shared: &Shared, req: &Request<Incoming>) -> Res {
    let (_, me) = match signed_in(shared, req, true) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    if me.access != Access::Control {
        return error(
            403,
            "this is a view link: signing everyone out needs a control link",
        );
    }
    shared.auth.revoke_all();
    let mut res = reply(204, "application/json", Bytes::new());
    res.headers_mut().insert(
        SET_COOKIE,
        value(&format!(
            "{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
        )),
    );
    res
}

/// A device that may start sessions: signed in with control access.
fn controller(shared: &Shared, req: &Request<Incoming>) -> Result<crate::auth::Device, Box<Res>> {
    let (_, device) = signed_in(shared, req, true)?;
    if device.access != Access::Control {
        return Err(Box::new(error(
            403,
            "this is a view link: starting a session needs a control link",
        )));
    }
    Ok(device)
}

fn start_error(why: StartError) -> Res {
    match why {
        StartError::Invalid(why) => error(400, &why),
        StartError::Refused(why) => error(403, &why),
        StartError::Failed(why) => error(500, &why),
    }
}

#[derive(Deserialize)]
struct FoldersBody {
    #[serde(default)]
    under: Option<String>,
}

async fn folders(shared: &Arc<Shared>, req: Request<Incoming>) -> Res {
    if let Err(res) = controller(shared, &req) {
        return *res;
    }
    let bytes = match body(req).await {
        Ok(bytes) => bytes,
        Err(res) => return *res,
    };
    let Ok(wanted) = serde_json::from_slice::<FoldersBody>(&bytes) else {
        return error(400, "send {} or {\"under\": \"<folder>\"}");
    };
    let under = wanted.under.filter(|under| !under.trim().is_empty());
    match shared.sessions.folders(under.as_deref()).await {
        Ok(folders) => json_reply(200, serde_json::to_value(folders).unwrap_or_default()),
        Err(why) => start_error(why),
    }
}

async fn start(shared: &Arc<Shared>, req: Request<Incoming>) -> Res {
    let device = match controller(shared, &req) {
        Ok(device) => device,
        Err(res) => return *res,
    };
    let bytes = match body(req).await {
        Ok(bytes) => bytes,
        Err(res) => return *res,
    };
    let Ok(new) = serde_json::from_slice::<NewSession>(&bytes) else {
        return error(
            400,
            "send {\"cwd\": \"<folder>\"}, with `prompt` and `mode` if you like",
        );
    };
    if new.cwd.trim().is_empty() {
        return error(400, "`cwd` names the folder the session works in");
    }
    if let Some(mode) = &new.mode
        && let Err(why) = crate::check(
            Access::Control,
            &json!({ "type": "set_mode", "mode": mode }),
            shared.grants,
        )
    {
        return error(403, &why);
    }
    if !shared.may_start(&device.id) {
        let mut res = error(429, "too many sessions started: wait a minute");
        res.headers_mut().insert(RETRY_AFTER, value("60"));
        return res;
    }
    match shared.sessions.start(new).await {
        Ok(id) => json_reply(200, json!({ "id": id })),
        Err(why) => start_error(why),
    }
}

fn socket(shared: &Arc<Shared>, req: &mut Request<Incoming>, id: &str) -> Res {
    let device = match authenticate(shared, req, true) {
        Ok(device) => device.map(|(_, device)| device),
        Err(res) => return *res,
    };
    let wants = |name: &str, wanted: &str| {
        header(req, name).is_some_and(|v| v.split(',').any(|part| part.trim().eq_ignore_ascii_case(wanted)))
    };
    let Some(key) = header(req, "sec-websocket-key").map(str::to_string) else {
        return error(400, "this route is a WebSocket");
    };
    if !wants("upgrade", "websocket")
        || !wants("connection", "upgrade")
        || header(req, "sec-websocket-version") != Some("13")
    {
        return error(400, "this route is a WebSocket (version 13)");
    }
    let accept = derive_accept_key(key.as_bytes());
    let upgraded = hyper::upgrade::on(&mut *req);
    let (shared, id) = (shared.clone(), id.to_string());
    tokio::spawn(async move {
        let Ok(upgraded) = upgraded.await else { return };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MESSAGE_LIMIT))
            .max_frame_size(Some(MESSAGE_LIMIT));
        let socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;
        ws::run(shared, socket, device, id).await;
    });
    let mut res = Response::new(Full::default());
    *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = res.headers_mut();
    headers.insert(UPGRADE, value("websocket"));
    headers.insert(CONNECTION, value("Upgrade"));
    headers.insert("sec-websocket-accept", value(&accept));
    res
}
