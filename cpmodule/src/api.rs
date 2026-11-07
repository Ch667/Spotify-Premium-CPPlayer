//! Provider 标准 API 分发 + axum 本地服务。
//!
//! 路由：
//! - `POST /api/{method}` 与 `POST /{method}`（指南 §3.2 要求双路径兼容）
//! - `GET  /stream/{trackId}`  WAV 音频流（支持 Range seek）
//! - `GET  /health`            健康检查

use std::sync::Arc;

use axum::extract::Path;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::engine::Core;
use crate::stream::BYTES_PER_MS;

const WEB_API: &str = "https://api.spotify.com/v1";

pub fn router() -> Router {
    let core = Core::global();
    Router::new()
        .route("/health", get(health))
        .route("/api/{*method}", post(call_api))
        .route("/{*method}", post(call_api))
        .route("/stream/{id}", get(stream_track))
        .with_state(core.inner.clone())
}

/// 绑定并后台运行服务，返回实际端口（同步返回 = 已就绪可服务）。
/// 实例统一取 `Core::global()`，保证 stream_cell / port / 会话全局唯一。
pub async fn serve(port: u16) -> Result<u16, String> {
    let core = Core::global();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("bind {port}: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    core.inner
        .port
        .store(bound, std::sync::atomic::Ordering::SeqCst);

    let app = router();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("server exited: {e}");
        }
    });
    Ok(bound)
}

// ---------------------------------------------------------------- HTTP 处理器

async fn health() -> Json<Value> {
    Json(json!({"code": 200, "status": "ok"}))
}

async fn call_api(Path(method): Path<String>, body: String) -> Response {
    let params: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let core = Core::global();
    let resp = dispatch(core, &method, params).await;
    Json(resp).into_response()
}

/// Range: `bytes=a-b` / `bytes=a-` / `bytes=-suffix`；只取起点（seek 语义，见 stream.rs）。
fn parse_range(h: Option<&HeaderValue>) -> Option<u64> {
    let raw = h?.to_str().ok()?.strip_prefix("bytes=")?.to_string();
    if let Some(start) = raw.strip_suffix('-') {
        return start.parse::<u64>().ok();
    }
    if let Some(suffix) = raw.strip_prefix('-') {
        let _ = suffix; // 后缀区间对「按位置重启」的流无意义，忽略
        return None;
    }
    raw.split('-')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
}

async fn stream_track(Path(track_id): Path<String>, headers: HeaderMap) -> Response {
    let core = Core::global();
    // 每次带 Range 的请求都从对应位置重启播放（seek 语义）。
    // 起点先用一个宽松上限做截断，实际总长由新流决定。
    let start_bytes = parse_range(headers.get(header::RANGE)).unwrap_or(0);
    let position_ms = start_bytes.saturating_sub(44) / BYTES_PER_MS;

    let stream = match core.inner.play_from(&track_id, position_ms.min(u32::MAX as u64) as u32).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"code": 502, "msg": e})),
            )
                .into_response()
        }
    };

    let total = stream.total();
    let body_start = 44u64;
    let is_partial = position_ms > 0 && start_bytes > body_start && start_bytes < total;
    let status = if is_partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut resp = (
        status,
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::ACCEPT_RANGES, "bytes"),
        ],
        axum::body::Body::from_stream(stream.read_sequential(body_start, total)),
    )
        .into_response();

    let len = total - body_start;
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if is_partial {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {body_start}-{}/{}", total - 1, total)) {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
    }
    resp
}

// ---------------------------------------------------------------- API 分发

pub async fn dispatch(core: &Core, method: &str, params: Value) -> Value {
    log::debug!("api {method} {params}");
    match method {
        // apiMap 同时映射 song/url/v1 与 song/url/v1/302 → song/url
        "song/url" | "song/url/v1" | "song/url/v1/302" => song_url(core, params).await,
        "search" | "cloudsearch" => search(core, params).await,
        "song/detail" => song_detail(core, params).await,
        "lyric" | "lyric/new" => lyric(),
        "scrobble" => json!({"code": 200}),
        // ---- 宿主账户页登录通道（NCM 形状，AccountScreen 按 manifest.loginMethods 生成）----
        "login/qr/key" => json!({"code": 200, "data": {"unikey": qr_key()}}),
        "login/qr/create" => qr_create(core).await,
        "login/qr/check" => qr_check(core).await,
        "login/status" => login_status(core).await,
        "logout" => logout(core).await,
        // ---- 调试/备用通道（宿主 UI 不直接调用）----
        "auth/login" => auth_login(core).await,
        "auth/status" => auth_status(core).await,
        "auth/logout" => auth_logout(core),
        _ => json!({"code": 404, "msg": format!("unsupported method: {method}")}),
    }
}

fn get_id(params: &Value) -> Option<String> {
    params.get("id").map(|v| match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }).filter(|s| !s.is_empty())
}

async fn song_url(core: &Core, params: Value) -> Value {
    let Some(id) = get_id(&params) else {
        return json!({"code": 400, "msg": "missing id"});
    };
    // 无版权/不可播 → url 为 null（指南 §6 响应约定）
    match core.inner.prepare(&id).await {
        Ok((url, _duration)) => json!({
            "code": 200,
            "data": [{"id": id, "url": url, "br": 320000, "type": "wav"}]
        }),
        Err(e) => json!({"code": 200, "data": [{"id": id, "url": null, "br": 0}], "msg": e}),
    }
}

/// Web API 搜索 → 网易云形态（ar/al/dt，指南 §7.1）。
async fn search(core: &Core, params: Value) -> Value {
    let Some(keywords) = params.get("keywords").and_then(|v| v.as_str()) else {
        return json!({"code": 400, "msg": "missing keywords"});
    };
    let stype = params.get("type").and_then(|v| v.as_str()).unwrap_or("1");
    let token = match web_token(core).await {
        Ok(t) => t,
        Err(e) => return json!({"code": 401, "msg": e}),
    };

    match stype {
        "1" => {
            let url = format!("{WEB_API}/search?q={}&type=track&limit=30", urlencode(keywords));
            match core.inner.http().get(url).bearer(&token).send().await {
                Ok(r) => match r.json::<Value>().await {
                    Ok(v) => {
                        let tracks = v["tracks"]["items"].as_array().cloned().unwrap_or_default();
                        let songs: Vec<Value> = tracks.iter().map(map_track).collect();
                        let n = songs.len();
                        json!({"code": 200, "result": {"songs": songs, "songCount": n}, "more": !songs.is_empty()})
                    }
                    Err(e) => json!({"code": 502, "msg": format!("decode: {e}")}),
                },
                Err(e) => json!({"code": 502, "msg": format!("search: {e}")}),
            }
        }
        "1000" => {
            let url = format!("{WEB_API}/search?q={}&type=playlist&limit=20", urlencode(keywords));
            match core.inner.http().get(url).bearer(&token).send().await {
                Ok(r) => match r.json::<Value>().await {
                    Ok(v) => {
                        let items = v["playlists"]["items"].as_array().cloned().unwrap_or_default();
                        let playlists: Vec<Value> = items
                            .iter()
                            .filter_map(|p| {
                                let id = p["id"].as_str()?;
                                Some(json!({
                                    "id": id,
                                    "name": p["name"],
                                    "coverImgUrl": p["images"][0]["url"],
                                    "trackCount": p["tracks"]["total"],
                                    "creator": {"nickname": p["owner"]["display_name"]},
                                }))
                            })
                            .collect();
                        let n = playlists.len();
                        json!({"code": 200, "result": {"playlists": playlists, "playlistCount": n}})
                    }
                    Err(e) => json!({"code": 502, "msg": format!("decode: {e}")}),
                },
                Err(e) => json!({"code": 502, "msg": format!("search: {e}")}),
            }
        }
        _ => json!({"code": 200, "result": {}, "msg": "unsupported search type"}),
    }
}

/// Web API /v1/tracks → `{"code":200,"songs":[...]}`（指南 §6）。
async fn song_detail(core: &Core, params: Value) -> Value {
    let ids = params
        .get("ids")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if ids.is_empty() {
        return json!({"code": 400, "msg": "missing ids"});
    }
    let token = match web_token(core).await {
        Ok(t) => t,
        Err(e) => return json!({"code": 401, "msg": e}),
    };
    let url = format!("{WEB_API}/tracks?ids={}", urlencode(ids));
    match core.inner.http().get(url).bearer(&token).send().await {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => {
                let tracks = v["tracks"].as_array().cloned().unwrap_or_default();
                let songs: Vec<Value> = tracks.iter().filter(|t| !t.is_null()).map(map_track).collect();
                json!({"code": 200, "songs": songs})
            }
            Err(e) => json!({"code": 502, "msg": format!("decode: {e}")}),
        },
        Err(e) => json!({"code": 502, "msg": format!("detail: {e}")}),
    }
}

fn lyric() -> Value {
    // Spotify Web API 不提供同步/逐字歌词，返回空串由宿主容错（方案 §1.3）。
    json!({"code": 200, "lrc": {"lyric": ""}, "tlyric": {"lyric": ""}})
}

async fn auth_login(core: &Core) -> Value {
    match Arc::clone(&core.inner).auth_login_start().await {
        Ok((login_url, user_code)) => {
            json!({"code": 200, "data": {"loginUrl": login_url, "userCode": user_code}})
        }
        Err(e) => json!({"code": 500, "msg": e}),
    }
}

async fn auth_status(core: &Core) -> Value {
    match core.inner.ensure_session().await {
        Ok(s) => json!({"code": 200, "data": {"login": true, "user": s.username()}}),
        Err(e) => json!({"code": 200, "data": {"login": false}, "msg": e}),
    }
}

fn auth_logout(core: &Core) -> Value {
    let dir = &core.inner.data_dir;
    let removed = [
        dir.join("credentials"),
        dir.join("credentials.json"),
    ]
    .iter()
    .any(|p| {
        std::fs::remove_file(p).is_ok() || std::fs::remove_dir_all(p).is_ok()
    });
    json!({"code": 200, "data": {"removed": removed}})
}

// ------------------------------------------------ 宿主账户页登录（NCM 形状适配）

/// 形式 key：宿主 QR 流程要求先拿 unikey 再 create，本模块单用户槽位不校验。
fn qr_key() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("spotify{ms}")
}

/// login/qr/create：发起设备码流程。qrurl 用 verification_uri_complete
/// （验证码已预填），宿主 QrCodeImage(qrUrl) 扫码即直达授权确认页。
///
/// 另外顺手用系统浏览器打开同一个链接：这是网页地址，用 Spotify App 扫是扫不出来的，
/// 直接跳到浏览器才是这个设备码流程的正确姿势（见 `opener`）。
async fn qr_create(core: &Core) -> Value {
    match Arc::clone(&core.inner).auth_login_start().await {
        Ok((qrurl, user_code)) => {
            crate::opener::open_url(&qrurl);
            json!({
                "code": 200,
                "data": {"qrurl": qrurl, "userCode": user_code}
            })
        }
        Err(e) => json!({"code": 500, "msg": e}),
    }
}

/// login/qr/check：登录进度 → NCM 状态码。
/// 801/802 等待（宿主文案「请在手机上确认」）、803 成功（带 cookie 占位）、800 过期。
async fn qr_check(core: &Core) -> Value {
    use crate::engine::{LOGIN_DONE, LOGIN_IDLE, LOGIN_PENDING};
    match core.inner.login_state.load(std::sync::atomic::Ordering::SeqCst) {
        LOGIN_DONE => json!({"code": 803, "cookie": "spotify=ok"}),
        LOGIN_PENDING => {
            // 有进行中的枚码才报等待；槽位已空（刚过期）按过期处理。
            if core.inner.login.lock().await.is_some() {
                json!({"code": 802, "msg": "请在任意浏览器打开二维码并确认授权"})
            } else {
                json!({"code": 800})
            }
        }
        LOGIN_IDLE => json!({"code": 800}),
        _ => json!({"code": 800}),
    }
}

/// login/status：⚠️ NCM 特殊形状——code 在 data 里、顶层无 code、
/// 未登录也返回 200 + account/profile 为 null；宿主只看 uid（LoginStatus.kt）。
async fn login_status(core: &Core) -> Value {
    let (logged, uid, name) = core.inner.login_status().await;
    if logged {
        json!({
            "data": {
                "code": 200,
                "account": {"id": uid, "type": 1, "status": 0},
                "profile": {"userId": uid, "nickname": name, "avatarUrl": Value::Null}
            }
        })
    } else {
        json!({"data": {"code": 200, "account": Value::Null, "profile": Value::Null}})
    }
}

async fn logout(core: &Core) -> Value {
    let removed = core.inner.logout().await;
    json!({"code": 200, "data": {"removed": removed}})
}

// ---------------------------------------------------------------- Web API 工具

async fn web_token(core: &Core) -> Result<String, String> {
    let session = core.inner.ensure_session().await?;
    let token = session
        .token_provider()
        .get_token("streaming")
        .await
        .map_err(|e| format!("token: {e}"))?;
    Ok(token.access_token)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn map_track(t: &Value) -> Value {
    let uri = t["uri"].as_str().unwrap_or_default();
    let id = uri.strip_prefix("spotify:track:").unwrap_or(uri);
    json!({
        "id": id,
        "name": t["name"],
        "ar": t["artists"].as_array().map(|artists| {
            artists.iter().map(|a| json!({"id": a["id"], "name": a["name"]})).collect::<Vec<_>>()
        }).unwrap_or_default(),
        "al": {
            "id": t["album"]["id"],
            "name": t["album"]["name"],
            "picUrl": t["album"]["images"][0]["url"],
        },
        "dt": t["duration_ms"],
    })
}

trait Bearer {
    fn bearer(self, token: &str) -> reqwest::RequestBuilder;
}

impl Bearer for reqwest::RequestBuilder {
    fn bearer(self, token: &str) -> reqwest::RequestBuilder {
        self.header(header::AUTHORIZATION, format!("Bearer {token}"))
    }
}
