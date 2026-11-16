//! CPPlayer `kmp-sdk-v1` 原生 Provider：把 Spotify 引擎接到新协议上。
//!
//! 宿主（v1.8.8 起）只认 `Java_cp_player_sdk_jni_NativeSession_*` 这组符号和位置式 Wire 编码，
//! 旧的 `nativeCallApi` JSON 协议已被删除。本文件实现 [`MusicProvider`]，
//! 由 `export_provider!` 导出固定符号；引擎（会话、播放、WAV 流）原样复用。
//!
//! 登录：Spotify 设备码流程是**网页授权**（`spotify.com/pair?code=…`），
//! 用 `webview.url` 返回授权页地址，宿主用系统浏览器打开；`webview.check` 轮询结果。
//! 同时保留 `qr.*` 三个动作，兼容仍走二维码通道的界面。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpplayer_provider_sdk::*;
use serde_json::Value;

use crate::engine::{Core, LOGIN_DONE, LOGIN_IDLE, LOGIN_PENDING};

const WEB_API: &str = "https://api.spotify.com/v1";
/// 登录成功后交给宿主保存的占位凭据。
///
/// 真正的 Spotify 凭据由模块自己存在 `user_data/`（librespot 的 credentials 缓存），
/// 宿主只需要一个非空字符串来表示「这个音源已登录」。内容不含任何秘密。
const SESSION_MARK: &str = "spotify-session";

pub struct SpotifyProvider;

pub fn create(_context: &ProviderContext) -> Box<dyn MusicProvider> {
    Box::new(SpotifyProvider)
}

#[cfg(feature = "sdk")]
cpplayer_provider_sdk::export_provider!(create);

// ---------------------------------------------------------------- 工具

fn core() -> &'static Core {
    Core::global()
}

/// 起本地流服务（幂等）。宿主会并发调用 resolve，所以用锁串行化「检查 + 绑定」，
/// 否则两个线程都看到端口为 0，会各绑一个，后绑的覆盖端口号，先返回的 URL 就指向已无人服务的端口。
fn ensure_stream_server() -> ProviderResult<()> {
    static START: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = START.lock().unwrap_or_else(|e| e.into_inner());
    if core().inner.port.load(Ordering::SeqCst) != 0 {
        return Ok(());
    }
    core()
        .runtime
        .block_on(crate::api::serve(0))
        .map(|_| ())
        .map_err(|e| Failure::new(FailureCategory::Runtime, format!("本地流服务启动失败: {e}")))
}

fn fail<T>(category: FailureCategory, message: impl Into<String>) -> ProviderResult<T> {
    Err(Failure::new(category, message))
}

/// 引擎错误串 → 宿主失败类别。未登录统一归 `Authentication`，宿主据此提示去登录。
fn classify(message: String) -> Failure {
    let lower = message.to_lowercase();
    let category =
        if lower.contains("not logged in") || lower.contains("token") && lower.contains("401") {
            FailureCategory::Authentication
        } else if lower.contains("timed out") || lower.contains("timeout") {
            FailureCategory::Timeout
        } else if lower.contains("429") || lower.contains("rate") {
            FailureCategory::RateLimit
        } else if lower.contains("decode") {
            FailureCategory::InvalidResponse
        } else if lower.contains("connect") || lower.contains("dns") || lower.contains("network") {
            FailureCategory::Network
        } else {
            FailureCategory::Runtime
        };
    Failure::new(category, message)
}

fn scope_for_url(url: &str) -> Option<&'static str> {
    let path = url
        .strip_prefix(WEB_API)
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    match path {
        "/me/playlists" => Some("playlist-read-private"),
        "/me/albums" => Some("user-library-read"),
        "/me" => Some("user-read-private"),
        _ => None,
    }
}

async fn web_token(scope: Option<&str>) -> Result<String, String> {
    if let Some(token) = core().inner.oauth.access_token(scope).await? {
        return Ok(token);
    }
    // Legacy installs have AP credentials but no saved OAuth grant. Do not use
    // librespot's generic token cache for user-library endpoints: its scope set
    // may be streaming-only, and this can't safely prove requested grants.
    if scope.is_some() && core().inner.oauth.requires_scope_upgrade() {
        return Err("Spotify 旧授权缺少个人曲库权限，请重新进行浏览器登录".into());
    }
    let session = core().inner.ensure_session().await?;
    let scopes = match scope {
        Some(scope) => format!("streaming,{scope}"),
        None => "streaming".to_owned(),
    };
    let token = session
        .token_provider()
        .get_token(&scopes)
        .await
        .map_err(|_| "Spotify Web API 授权不可用，请重新进行浏览器登录".to_string())?;
    Ok(token.access_token)
}

/// GET 一个 Web API 端点并解成 JSON。HTTP 状态码按语义映射成失败类别。
fn web_get(url: String) -> ProviderResult<Value> {
    core().runtime.block_on(async move {
        let token = web_token(scope_for_url(&url)).await.map_err(|_| {
            Failure::new(
                FailureCategory::Authentication,
                "Spotify Web API 授权无效或缺少权限，请重新登录",
            )
        })?;
        let resp = core()
            .inner
            .http()
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| classify(format!("network: {e}")))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return fail(
                FailureCategory::Authentication,
                format!("Spotify 拒绝访问（HTTP {status}）"),
            );
        }
        if status.as_u16() == 404 {
            return fail(FailureCategory::NotFound, "未找到");
        }
        if status.as_u16() == 429 {
            return fail(FailureCategory::RateLimit, "请求过于频繁，请稍后重试");
        }
        if !status.is_success() {
            return fail(
                FailureCategory::Network,
                format!("Spotify 返回 HTTP {status}"),
            );
        }
        resp.json::<Value>()
            .await
            .map_err(|e| Failure::new(FailureCategory::InvalidResponse, format!("decode: {e}")))
    })
}

/// Spotify caps individual pages at 50, while CPPlayer asks for up to 500.
/// Accumulate raw slots (including unavailable/null entries) so filtered items do
/// not shift subsequent offsets or silently discard the rest of the library.
fn library_page(
    endpoint: &str,
    limit: i32,
    offset: i32,
    mut fetch: impl FnMut(String) -> ProviderResult<Value>,
) -> ProviderResult<Value> {
    let wanted = limit.clamp(1, 500) as usize;
    let mut items = Vec::new();
    let mut total = None;
    let mut more = false;
    while items.len() < wanted {
        let count = (wanted - items.len()).min(50);
        let page = fetch(format!(
            "{WEB_API}{endpoint}?limit={count}&offset={}",
            offset.max(0) as usize + items.len()
        ))?;
        let raw = page["items"].as_array().ok_or_else(|| {
            Failure::new(
                FailureCategory::InvalidResponse,
                "Spotify library items missing",
            )
        })?;
        if raw.len() > count {
            return fail(
                FailureCategory::InvalidResponse,
                "Spotify library page exceeds limit",
            );
        }
        total = page["total"].as_i64().or(total);
        more = page["next"].as_str().is_some_and(|s| !s.is_empty());
        if raw.is_empty() {
            more = false;
            break;
        }
        items.extend(raw.iter().cloned());
        if !more {
            break;
        }
    }
    Ok(serde_json::json!({"items":items, "total":total, "next":if more {Some("more")} else {None}}))
}

fn album_track_page(album_id: &str, limit: i32, offset: i32) -> ProviderResult<Value> {
    let limit = limit.clamp(1, 50);
    let mut items = Vec::new();
    let mut total = None;
    let mut next_offset = offset.max(0);
    while items.len() < limit as usize {
        let count = (limit as usize - items.len()).min(50) as i32;
        let url = format!(
            "{WEB_API}/albums/{}/tracks?limit={count}&offset={next_offset}",
            urlencode(album_id)
        );
        let page = web_get(url)?;
        let page_items = page["items"].as_array().ok_or_else(|| {
            Failure::new(
                FailureCategory::InvalidResponse,
                "Spotify album tracks missing",
            )
        })?;
        if page_items.len() > count as usize {
            return fail(
                FailureCategory::InvalidResponse,
                "Spotify album track page exceeds limit",
            );
        }
        total = page["total"].as_i64().or(total);
        if page_items.is_empty() {
            break;
        }
        let count = page_items.len() as i32;
        items.extend(page_items.iter().cloned());
        next_offset = next_offset.saturating_add(count);
        if page["next"].is_null() {
            break;
        }
    }
    Ok(
        serde_json::json!({"items":items, "total":total, "next":if total.is_some_and(|n| i64::from(next_offset) < n) {Some("more")} else {None}}),
    )
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

fn str_of(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn cover_of(v: &Value) -> Option<String> {
    str_of(&v["images"][0]["url"])
}

fn artists_of(v: &Value) -> Vec<Artist> {
    v["artists"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Artist {
                        id: str_of(&x["id"]),
                        name: str_of(&x["name"])?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Web API 的曲目对象 → 宿主 Track。`album`/`cover` 在专辑与歌单里由父级补。
fn track_of(t: &Value, album: Option<&str>, cover: Option<&str>) -> Option<Track> {
    let id = str_of(&t["id"]).or_else(|| {
        t["uri"]
            .as_str()
            .and_then(|u| u.strip_prefix("spotify:track:"))
            .map(str::to_owned)
    })?;
    let title = str_of(&t["name"])?;
    let album_name = str_of(&t["album"]["name"]).or_else(|| album.map(str::to_owned));
    let cover_url = cover_of(&t["album"]).or_else(|| cover.map(str::to_owned));
    Some(Track {
        id,
        title,
        artists: artists_of(t),
        duration_ms: t["duration_ms"].as_i64().unwrap_or(0).max(0),
        album: album_name,
        cover_url,
    })
}

fn album_of(a: &Value) -> Option<Album> {
    Some(Album {
        id: str_of(&a["id"])?,
        title: str_of(&a["name"])?,
        artists: artists_of(a),
        cover_url: cover_of(a),
        track_count: a["total_tracks"].as_i64().map(|n| n as i32),
        released_at_epoch_ms: None,
        company: str_of(&a["label"]),
        description: None,
    })
}

fn clamp_limit(limit: i32) -> i32 {
    limit.clamp(1, 50)
}

fn page_flags(body: &Value, got: usize, offset: i32) -> (Option<i64>, Option<bool>) {
    let total = body["total"].as_i64();
    // `next` 非空一定还有下一页；否则用 total 与已取数量比较。
    let has_more = if !body["next"].is_null() {
        true
    } else {
        total.is_some_and(|t| {
            i64::from(offset.max(0)) + got as i64 > 0 && i64::from(offset.max(0)) + (got as i64) < t
        })
    };
    (total, Some(has_more))
}

// ---------------------------------------------------------------- Provider

impl MusicProvider for SpotifyProvider {
    fn capabilities(&self, _context: &ProviderContext) -> Vec<&'static str> {
        vec![
            Capability::SEARCH,
            Capability::GET,
            Capability::RESOLVE,
            Capability::SEARCH_ALBUMS,
            Capability::GET_ALBUM,
            Capability::SEARCH_ARTISTS,
            Capability::GET_ARTIST,
            Capability::GET_ARTIST_TRACKS,
            Capability::GET_ARTIST_ALBUMS,
            Capability::SEARCH_PLAYLISTS,
            Capability::GET_PLAYLIST,
            Capability::GET_PLAYLIST_TRACKS,
            Capability::GET_CURRENT_ACCOUNT,
            Capability::GET_USER_PROFILE,
            Capability::GET_USER_PLAYLISTS,
            Capability::GET_USER_ALBUMS,
            Capability::AUTHENTICATE,
        ]
    }

    fn search(
        &mut self,
        request: SearchRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<TrackPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/search?q={}&type=track&limit={limit}&offset={}",
            urlencode(&request.keyword),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        let tracks = &body["tracks"];
        let items: Vec<Track> = tracks["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| track_of(t, None, None)).collect())
            .unwrap_or_default();
        let (total, has_more) = page_flags(tracks, items.len(), request.offset);
        Ok(TrackPage {
            items,
            total,
            has_more,
        })
    }

    fn get(&mut self, request: IdRequest, _c: &ProviderContext) -> ProviderResult<Track> {
        let body = web_get(format!("{WEB_API}/tracks/{}", urlencode(&request.id)))?;
        track_of(&body, None, None)
            .ok_or_else(|| Failure::new(FailureCategory::NotFound, "曲目不存在"))
    }

    fn resolve(
        &mut self,
        request: ResolveRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<MediaResource> {
        // 先让引擎校验曲目可播并预取时长，再返回本地 WAV 流地址。
        // 真正的解码推迟到播放器发起 /stream 请求，避免对着空气解码。
        let id = request.id.clone();
        // 新协议没有 startNativeServer 入口：第一次 resolve 时在回环地址上起流服务（端口由系统分配）。
        // 返回的地址是 127.0.0.1，音频与 API 都不出本机。
        ensure_stream_server()?;
        let (url, _duration) = core()
            .runtime
            .block_on(async { core().inner.prepare(&id).await })
            .map_err(classify)?;
        Ok(MediaResource {
            url,
            actual_quality: request.quality,
            headers: Vec::new(),
            expires_at_epoch_ms: None,
            format: Some("wav".into()),
            bitrate: Some(1411),
            size_bytes: None,
        })
    }

    fn search_albums(
        &mut self,
        request: SearchRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<AlbumPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/search?q={}&type=album&limit={limit}&offset={}",
            urlencode(&request.keyword),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        let albums = &body["albums"];
        let items: Vec<Album> = albums["items"]
            .as_array()
            .map(|a| a.iter().filter_map(album_of).collect())
            .unwrap_or_default();
        let (total, has_more) = page_flags(albums, items.len(), request.offset);
        Ok(AlbumPage {
            items,
            total,
            has_more,
        })
    }

    fn get_album(
        &mut self,
        request: IdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<AlbumDetails> {
        let mut body = web_get(format!("{WEB_API}/albums/{}", urlencode(&request.id)))?;
        let album =
            album_of(&body).ok_or_else(|| Failure::new(FailureCategory::NotFound, "专辑不存在"))?;
        let name = album.title.clone();
        let cover = album.cover_url.clone();
        if body["tracks"]["next"].is_string() {
            body["tracks"] = album_track_page(&request.id, 50, 0)?;
        }
        let tracks = body["tracks"]["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        let mut tr = track_of(t, Some(&name), cover.as_deref())?;
                        // 专辑内曲目对象的 artists 在 simplified 形态里已有
                        if tr.artists.is_empty() {
                            tr.artists = album.artists.clone();
                        }
                        Some(tr)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AlbumDetails { album, tracks })
    }

    fn search_artists(
        &mut self,
        request: SearchRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<ArtistPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/search?q={}&type=artist&limit={limit}&offset={}",
            urlencode(&request.keyword),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        let artists = &body["artists"];
        let items: Vec<CatalogArtist> = artists["items"]
            .as_array()
            .map(|a| a.iter().filter_map(catalog_artist_of).collect())
            .unwrap_or_default();
        let (total, has_more) = page_flags(artists, items.len(), request.offset);
        Ok(ArtistPage {
            items,
            total,
            has_more,
        })
    }

    fn get_artist(
        &mut self,
        request: IdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<CatalogArtist> {
        let body = web_get(format!("{WEB_API}/artists/{}", urlencode(&request.id)))?;
        catalog_artist_of(&body)
            .ok_or_else(|| Failure::new(FailureCategory::NotFound, "歌手不存在"))
    }

    fn get_artist_tracks(
        &mut self,
        request: PagedIdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<TrackPage> {
        // Spotify 只提供「热门单曲」（最多 10 首），不分页。
        let body = web_get(format!(
            "{WEB_API}/artists/{}/top-tracks",
            urlencode(&request.id)
        ))?;
        let items: Vec<Track> = body["tracks"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| track_of(t, None, None)).collect())
            .unwrap_or_default();
        let total = Some(items.len() as i64);
        Ok(TrackPage {
            items,
            total,
            has_more: Some(false),
        })
    }

    fn get_artist_albums(
        &mut self,
        request: PagedIdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<AlbumPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/artists/{}/albums?include_groups=album,single&limit={limit}&offset={}",
            urlencode(&request.id),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        let items: Vec<Album> = body["items"]
            .as_array()
            .map(|a| a.iter().filter_map(album_of).collect())
            .unwrap_or_default();
        let (total, has_more) = page_flags(&body, items.len(), request.offset);
        Ok(AlbumPage {
            items,
            total,
            has_more,
        })
    }

    fn search_playlists(
        &mut self,
        request: SearchRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<PlaylistPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/search?q={}&type=playlist&limit={limit}&offset={}",
            urlencode(&request.keyword),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        let playlists = &body["playlists"];
        // 搜索结果里偶尔夹着 null 条目，必须过滤。
        let items: Vec<Playlist> = playlists["items"]
            .as_array()
            .map(|a| a.iter().filter_map(playlist_of).collect())
            .unwrap_or_default();
        let (total, has_more) = page_flags(playlists, items.len(), request.offset);
        Ok(PlaylistPage {
            items,
            total,
            has_more,
        })
    }

    fn get_playlist(
        &mut self,
        request: IdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<Playlist> {
        let body = web_get(format!("{WEB_API}/playlists/{}", urlencode(&request.id)))?;
        playlist_of(&body).ok_or_else(|| Failure::new(FailureCategory::NotFound, "歌单不存在"))
    }

    fn get_playlist_tracks(
        &mut self,
        request: PagedIdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<TrackPage> {
        let limit = clamp_limit(request.limit);
        let url = format!(
            "{WEB_API}/playlists/{}/tracks?limit={limit}&offset={}",
            urlencode(&request.id),
            request.offset.max(0)
        );
        let body = web_get(url)?;
        // 条目形如 {"track": {...}}（新接口可能是 {"item": {...}}）；本地文件 / 下架曲目没有 id，跳过。
        let items: Vec<Track> = body["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|it| {
                        let t = if it["track"].is_object() {
                            &it["track"]
                        } else {
                            &it["item"]
                        };
                        if !t.is_object() {
                            return None;
                        }
                        track_of(t, None, None)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (total, has_more) = page_flags(&body, items.len(), request.offset);
        Ok(TrackPage {
            items,
            total,
            has_more,
        })
    }

    fn get_user_playlists(
        &mut self,
        request: PagedIdRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<PlaylistPage> {
        require_login(_c)?;
        let body = library_page("/me/playlists", request.limit, request.offset, web_get)?;
        let items: Vec<Playlist> = body["items"]
            .as_array()
            .map(|a| a.iter().filter_map(playlist_of).collect())
            .unwrap_or_default();
        Ok(PlaylistPage {
            items,
            total: body["total"].as_i64(),
            has_more: Some(body["next"].as_str().is_some_and(|s| !s.is_empty())),
        })
    }

    fn get_user_albums(
        &mut self,
        request: UserListRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<AlbumPage> {
        require_login(_c)?;
        let body = library_page("/me/albums", request.limit, request.offset, web_get)?;
        // 收藏接口的每一项是 {"album": {...}}，不是裸专辑对象。
        let items: Vec<Album> = body["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|it| album_of(&it["album"])).collect())
            .unwrap_or_default();
        Ok(AlbumPage {
            items,
            total: body["total"].as_i64(),
            has_more: Some(body["next"].as_str().is_some_and(|s| !s.is_empty())),
        })
    }

    // ------------------------------------------------------------ 账号

    fn get_current_account(
        &mut self,
        _request: Bytes,
        context: &ProviderContext,
    ) -> ProviderResult<AccountSession> {
        // 宿主没有保存凭据 ⇒ 肯定没登录，不必去连 Spotify。
        if !context.is_logged_in() || core().inner.oauth.requires_scope_upgrade() {
            return Ok(AccountSession {
                logged_in: false,
                user_id: None,
                nickname: None,
                avatar_url: None,
            });
        }
        let (logged, _uid, _username) = core().runtime.block_on(core().inner.login_status());
        if !logged {
            return Ok(AccountSession {
                logged_in: false,
                user_id: None,
                nickname: None,
                avatar_url: None,
            });
        }
        // AP session usernames can be absent for token-authenticated Android
        // sessions. The host uses user_id to load its profile and per-account
        // library, so return Spotify's stable Web API identity instead.
        let profile = web_get(format!("{WEB_API}/me"))?;
        let id = str_of(&profile["id"]).ok_or_else(|| {
            Failure::new(
                FailureCategory::InvalidResponse,
                "Spotify profile identity missing",
            )
        })?;
        Ok(AccountSession {
            logged_in: true,
            user_id: Some(id),
            nickname: str_of(&profile["display_name"]),
            avatar_url: cover_of(&profile),
        })
    }

    fn get_user_profile(
        &mut self,
        request: UserRequest,
        context: &ProviderContext,
    ) -> ProviderResult<UserProfile> {
        require_login(context)?;
        // `accounts.session.userId` is the Web API ID returned by `/me`.
        // Reuse that exact opaque ID; AP usernames are not necessarily identical.
        let body = web_get(format!("{WEB_API}/users/{}", urlencode(&request.id)))?;
        let id = str_of(&body["id"]).ok_or_else(|| {
            Failure::new(
                FailureCategory::InvalidResponse,
                "Spotify profile identity missing",
            )
        })?;
        Ok(UserProfile {
            nickname: str_of(&body["display_name"]).unwrap_or_else(|| id.clone()),
            id,
            avatar_url: cover_of(&body),
            signature: None,
            playlist_count: 0,
            follows: 0,
            followeds: body["followers"]["total"]
                .as_i64()
                .unwrap_or(0)
                .clamp(0, i32::MAX as i64) as i32,
        })
    }

    fn authenticate(
        &mut self,
        request: AuthActionRequest,
        _c: &ProviderContext,
    ) -> ProviderResult<AuthActionResult> {
        match request.action.as_str() {
            // 浏览器授权：返回 spotify.com/pair?code=… ，宿主在系统浏览器里打开。
            "webview.url" | "qr.create" => start_login(request.action == "qr.create"),
            "qr.key" => Ok(AuthActionResult {
                success: true,
                code: Some(200),
                status: None,
                key: Some(login_key()),
                url: None,
                image_base64: None,
                cookie: None,
                verify_url: None,
                challenge: false,
                message: None,
            }),
            "webview.check" | "qr.check" => check_login(),
            "logout" => {
                core().runtime.block_on(core().inner.logout());
                Ok(ok_result())
            }
            other => fail(
                FailureCategory::Unsupported,
                format!("不支持的登录方式：{other}"),
            ),
        }
    }
}

/// 个人数据接口必须带登录态；未登录直接报 Authentication，宿主据此提示登录，而不是发一个必然 403 的请求。
fn require_login(context: &ProviderContext) -> ProviderResult<()> {
    if context.is_logged_in() {
        Ok(())
    } else {
        fail(FailureCategory::Authentication, "请先登录 Spotify")
    }
}

fn ok_result() -> AuthActionResult {
    AuthActionResult {
        success: true,
        code: Some(200),
        status: None,
        key: None,
        url: None,
        image_base64: None,
        cookie: None,
        verify_url: None,
        challenge: false,
        message: None,
    }
}

/// 形式 key：宿主 QR 流程要求先拿 key 再 create；本模块单用户槽位，不校验它。
fn login_key() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("spotify{ms}")
}

fn start_login(qr: bool) -> ProviderResult<AuthActionResult> {
    let inner = Arc::clone(&core().inner);
    let (url, user_code) = core()
        .runtime
        .block_on(inner.auth_login_start())
        .map_err(|e| Failure::new(FailureCategory::Authentication, e))?;
    // 设备码链接是网页，Spotify App 扫不了；顺手让系统浏览器直接打开（Android 有 JavaVM 时才生效）。
    crate::opener::open_url(&url);
    Ok(AuthActionResult {
        success: true,
        code: Some(200),
        status: Some(LOGIN_STATUS_WAITING),
        key: Some(if qr { login_key() } else { user_code.clone() }),
        url: Some(url),
        image_base64: None,
        cookie: None,
        verify_url: None,
        challenge: false,
        message: Some(format!("请在浏览器中确认授权，验证码 {user_code}")),
    })
}

/// 状态码沿用宿主的约定：800 过期 / 801 等待 / 802 已扫码待确认 / 803 成功。
const LOGIN_STATUS_WAITING: i32 = 801;

fn check_login() -> ProviderResult<AuthActionResult> {
    let inner = &core().inner;
    let state = inner.login_state.load(Ordering::SeqCst);
    let mut result = ok_result();
    match state {
        LOGIN_DONE => {
            result.code = Some(803);
            result.status = Some(803);
            // 凭据本体在模块 user_data/ 里；这里给宿主一个非空标记即可。
            result.cookie = Some(SESSION_MARK.to_owned());
        }
        LOGIN_PENDING => {
            let alive = core()
                .runtime
                .block_on(async { inner.login.lock().await.is_some() });
            let code = if alive { LOGIN_STATUS_WAITING } else { 800 };
            result.success = alive;
            result.code = Some(code);
            result.status = Some(code);
            result.message = Some("请在浏览器中完成授权".into());
        }
        LOGIN_IDLE | _ => {
            result.success = false;
            result.code = Some(800);
            result.status = Some(800);
            result.message = Some("授权已过期，请重新获取".into());
        }
    }
    Ok(result)
}

fn catalog_artist_of(a: &Value) -> Option<CatalogArtist> {
    let genres: Vec<String> = a["genres"]
        .as_array()
        .map(|g| {
            g.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(CatalogArtist {
        id: str_of(&a["id"])?,
        name: str_of(&a["name"])?,
        avatar_url: cover_of(a),
        description: if genres.is_empty() {
            None
        } else {
            Some(genres.join(" / "))
        },
        aliases: genres,
        album_count: None,
        track_count: None,
    })
}

fn playlist_of(p: &Value) -> Option<Playlist> {
    if !p.is_object() {
        return None;
    }
    Some(Playlist {
        id: str_of(&p["id"])?,
        title: str_of(&p["name"])?,
        cover_url: cover_of(p),
        track_count: p["tracks"]["total"].as_i64().map(|n| n as i32),
        creator_id: str_of(&p["owner"]["id"]),
        creator_name: str_of(&p["owner"]["display_name"]),
        description: str_of(&p["description"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpplayer_provider_sdk::{WireReader, WireWriter, dispatch};

    /// `login_state` 是进程内全局量，会改它的测试必须串行，否则互相踩。
    static LOGIN_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ctx(credential: Option<&str>) -> ProviderContext {
        ProviderContext {
            provider_id: "cp.provider.spotify.sdk".into(),
            account_id: "default".into(),
            credential: credential.map(str::to_owned),
            settings_json: "{}".into(),
            cancel: CancelToken::default(),
        }
    }

    fn call(op: i32, req: &[u8], c: &ProviderContext) -> ProviderResult<Vec<u8>> {
        let mut p = SpotifyProvider;
        dispatch(&mut p, op, req, c)
    }

    fn empty_bytes() -> Vec<u8> {
        let mut w = WireWriter::new();
        Bytes::default().write(&mut w).unwrap();
        w.finish()
    }

    fn auth_request(action: &str) -> Vec<u8> {
        let req = AuthActionRequest {
            action: action.into(),
            email: None,
            phone: None,
            password: None,
            captcha: None,
            md5: false,
            image_captcha: None,
            challenge_cookie: None,
            key: None,
            session_id: None,
            verify_id: None,
            extra_json: None,
            country_code: None,
        };
        let mut w = WireWriter::new();
        req.write(&mut w).unwrap();
        w.finish()
    }

    #[test]
    fn declares_the_capabilities_it_implements() {
        let out = call(1, &empty_bytes(), &ctx(None)).unwrap();
        let caps = Capabilities::read(&mut WireReader::new(&out).unwrap())
            .unwrap()
            .supported;
        for must in [
            "tracks.search",
            "tracks.resolve",
            "albums.get",
            "accounts.session",
            "auth.actions",
        ] {
            assert!(caps.iter().any(|c| c == must), "missing {must}: {caps:?}");
        }
        // 没实现的不能声明：评论 / 云盘之类应该是 Unsupported，而不是空页。
        assert!(!caps.iter().any(|c| c.starts_with("comments")));
    }

    #[test]
    fn logged_out_account_does_not_touch_the_network() {
        let out = call(59, &empty_bytes(), &ctx(None)).unwrap();
        let acc = AccountSession::read(&mut WireReader::new(&out).unwrap()).unwrap();
        assert!(!acc.logged_in);
        assert!(acc.user_id.is_none());
    }

    #[test]
    fn unknown_auth_action_is_unsupported_not_a_crash() {
        let err = call(60, &auth_request("login.phone"), &ctx(None)).unwrap_err();
        assert_eq!(err.category, FailureCategory::Unsupported);
    }

    #[test]
    fn qr_key_returns_a_nonempty_key() {
        let out = call(60, &auth_request("qr.key"), &ctx(None)).unwrap();
        let r = AuthActionResult::read(&mut WireReader::new(&out).unwrap()).unwrap();
        assert!(r.success);
        assert!(r.key.is_some_and(|k| k.starts_with("spotify")));
    }

    #[test]
    fn check_login_when_idle_reports_expired() {
        let _g = LOGIN_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 没有进行中的授权：必须报 800（过期），宿主据此提示重新获取，而不是一直转圈。
        core().inner.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
        let out = call(60, &auth_request("webview.check"), &ctx(None)).unwrap();
        let r = AuthActionResult::read(&mut WireReader::new(&out).unwrap()).unwrap();
        assert!(!r.success);
        assert_eq!(r.status, Some(800));
    }

    #[test]
    fn check_login_done_hands_the_host_a_credential() {
        let _g = LOGIN_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        core().inner.login_state.store(LOGIN_DONE, Ordering::SeqCst);
        let out = call(60, &auth_request("webview.check"), &ctx(None)).unwrap();
        let r = AuthActionResult::read(&mut WireReader::new(&out).unwrap()).unwrap();
        assert!(r.success);
        assert_eq!(r.status, Some(803));
        assert!(r.cookie.is_some_and(|c| !c.is_empty()));
        core().inner.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
    }

    #[test]
    fn personal_library_pages_are_bounded_to_spotify_api_limit_without_losing_items() {
        let mut calls = Vec::new();
        let body = library_page("/me/playlists", 120, 40, |url| {
            calls.push(url.clone());
            let offset = url.split("offset=").nth(1).unwrap().parse::<i32>().unwrap();
            let count = url.split("limit=").nth(1).unwrap().split('&').next().unwrap().parse::<i32>().unwrap();
            let items: Vec<_> = (offset..offset + count).map(|n| serde_json::json!({"id":n})).collect();
            Ok(serde_json::json!({"items":items,"total":200,"next":if offset + count < 200 {Some("next")} else {None}}))
        }).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls.iter().filter(|u| u.contains("limit=50")).count(), 2);
        assert!(calls[2].contains("limit=20"));
        assert!(calls[0].contains("offset=40"));
        assert!(calls[1].contains("offset=90"));
        assert!(calls[2].contains("offset=140"));
        assert_eq!(body["items"].as_array().unwrap().len(), 120);
        assert_eq!(body["items"][0]["id"], 40);
        assert_eq!(body["items"][119]["id"], 159);
        assert_eq!(body["total"], 200);
        assert!(body["next"].is_string());
    }

    #[test]
    fn personal_library_pages_preserve_raw_offsets_when_entries_are_filtered_later() {
        let body = library_page("/me/albums", 60, 100, |url| {
            let offset = url.split("offset=").nth(1).unwrap().parse::<i32>().unwrap();
            let count = url.split("limit=").nth(1).unwrap().split('&').next().unwrap().parse::<i32>().unwrap();
            let items: Vec<_> = (offset..offset + count).map(|n| if n == 105 {serde_json::Value::Null} else {serde_json::json!({"album":{"id":n}})}).collect();
            Ok(serde_json::json!({"items":items,"total":200,"next":if offset + count < 200 {Some("next")} else {None}}))
        }).unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 60);
        assert!(body["items"][5].is_null());
        assert_eq!(body["items"][10]["album"]["id"], 110);
    }

    #[test]
    fn page_flags_use_next_and_total() {
        let more = serde_json::json!({"total": 100, "next": "https://x"});
        assert_eq!(page_flags(&more, 20, 0), (Some(100), Some(true)));
        let last = serde_json::json!({"total": 25, "next": null});
        assert_eq!(page_flags(&last, 5, 20), (Some(25), Some(false)));
        let mid = serde_json::json!({"total": 100, "next": null});
        assert_eq!(page_flags(&mid, 20, 0), (Some(100), Some(true)));
    }

    #[test]
    fn track_mapping_skips_unplayable_entries() {
        let ok = serde_json::json!({"id":"abc","name":"S","artists":[{"id":"1","name":"A"}],"duration_ms":1000,"album":{"name":"Al","images":[{"url":"https://i/x.jpg"}]}});
        let t = track_of(&ok, None, None).unwrap();
        assert_eq!(
            (t.id.as_str(), t.album.as_deref(), t.duration_ms),
            ("abc", Some("Al"), 1000)
        );
        let no_id = serde_json::json!({"name":"local file","duration_ms":1});
        assert!(track_of(&no_id, None, None).is_none());
    }

    #[test]
    fn declares_user_playlist_and_album_capabilities() {
        let out = call(1, &empty_bytes(), &ctx(None)).unwrap();
        let caps = Capabilities::read(&mut WireReader::new(&out).unwrap())
            .unwrap()
            .supported;
        assert!(caps.iter().any(|c| c == "playlists.user"), "{caps:?}");
        assert!(caps.iter().any(|c| c == "accounts.albums"), "{caps:?}");
    }

    #[test]
    fn user_library_requires_login_and_never_hits_network() {
        // 未登录（没有 credential）时必须直接报 Authentication，不能去请求 /me/*。
        let mut p = SpotifyProvider;
        let req = PagedIdRequest {
            id: "me".into(),
            limit: 20,
            offset: 0,
        };
        let err = p.get_user_playlists(req, &ctx(None)).unwrap_err();
        assert_eq!(err.category, FailureCategory::Authentication);
        let ureq = UserListRequest {
            id: "me".into(),
            limit: 20,
            offset: 0,
        };
        let err = p.get_user_albums(ureq, &ctx(None)).unwrap_err();
        assert_eq!(err.category, FailureCategory::Authentication);
    }

    #[test]
    fn login_scopes_include_library_reads() {
        assert!(crate::engine::LOGIN_SCOPES.contains(&"user-library-read"));
        assert!(crate::engine::LOGIN_SCOPES.contains(&"playlist-read-private"));
        assert!(crate::engine::LOGIN_SCOPES.contains(&"streaming"));
    }
}
