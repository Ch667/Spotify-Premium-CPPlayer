//! SpotifyEngine：会话、播放器、当前流槽位与登录态管理。
//!
//! 线程模型：JNI 调用线程通过 `Core::runtime.block_on` 进入异步世界；
//! librespot 播放回调运行在自己的播放线程，经共享 `stream_cell` 交换数据。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_id::SpotifyId;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::{Metadata, Track};
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot_playback::mixer::NoOpVolume;
use librespot_playback::player::{Player, PlayerEvent};

use crate::stream::{StreamSink, TrackStream};

pub struct Core {
    pub runtime: tokio::runtime::Runtime,
    pub inner: Arc<Inner>,
}

pub struct Inner {
    /// 模块数据目录（凭据缓存、临时音频文件）。模块更新时宿主保留 user_data/。
    pub data_dir: PathBuf,
    pub oauth: crate::oauth_state::OAuthStore,
    /// 本地服务实际监听端口。
    pub port: AtomicU16,
    /// 当前音频流槽位（StreamSink 与 /stream 处理器共享）。
    pub stream_cell: Arc<tokio::sync::Mutex<Option<Arc<TrackStream>>>>,
    session: tokio::sync::Mutex<Option<Session>>,
    connect_gate: tokio::sync::Mutex<()>,
    player: tokio::sync::Mutex<Option<Arc<Player>>>,
    playback_change: tokio::sync::Mutex<()>,
    /// 进行中的设备码登录（url + user_code）。
    pub login: tokio::sync::Mutex<Option<(String, String)>>,
    /// 设备码登录进度：0=等待用户授权，1=成功，2=无进行中/已过期（见 LOGIN_* 常量）。
    pub login_state: AtomicU8,
    http: reqwest::Client,
}

/// 桌面 Keymaster 的 client_id。
///
/// Spotify 只对它开放设备码授权（实测：Android 的 `9a8d2f0c…` 回 `unauthorized_client`）。
/// librespot 在 Android 上默认选 Android 的 id，所以**设备码与会话都必须显式固定为它**，
/// 否则 `login/qr/create` 在手机上永远失败。会话用同一个 id，token 才能被接受
/// （见 librespot `login5.rs`：StoredCredential 走会话 client_id）。
pub const KEYMASTER_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

/// 设备码授权申请的 scope。`streaming` 用于播放，其余用于读取个人歌单与收藏专辑（`/me/*`）。
/// 实测桌面 Keymaster 接受整组；未识别的 scope 会让整次授权失败（`invalid_scope`），所以只列已验证的。
pub const LOGIN_SCOPES: &[&str] = &[
    "streaming",
    "playlist-read-private",
    "playlist-read-collaborative",
    "user-library-read",
    "user-read-private",
    "user-read-email",
    "user-follow-read",
];

/// 登录进度常量（与 api.rs 的 login/qr/check 状态映射配套）。
pub const LOGIN_PENDING: u8 = 0;
pub const LOGIN_DONE: u8 = 1;
pub const LOGIN_IDLE: u8 = 2;

impl Core {
    pub fn global() -> &'static Core {
        static CORE: OnceLock<Core> = OnceLock::new();
        CORE.get_or_init(Core::new)
    }

    fn new() -> Self {
        let data_dir = resolve_data_dir();
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            log::warn!("create data_dir {:?} failed: {e}", data_dir);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self {
            runtime,
            inner: Arc::new(Inner {
                oauth: crate::oauth_state::OAuthStore::new(&data_dir),
                data_dir,
                port: AtomicU16::new(0),
                stream_cell: Arc::new(tokio::sync::Mutex::new(None)),
                session: tokio::sync::Mutex::new(None),
                connect_gate: tokio::sync::Mutex::new(()),
                player: tokio::sync::Mutex::new(None),
                playback_change: tokio::sync::Mutex::new(()),
                login: tokio::sync::Mutex::new(None),
                login_state: AtomicU8::new(LOGIN_IDLE),
                http: reqwest::Client::new(),
            }),
        }
    }
}

impl Inner {
    pub fn temp_dir(&self) -> PathBuf {
        let p = self.data_dir.join("temp");
        let _ = std::fs::create_dir_all(&p);
        p
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 返回当前已连接的会话；没有则尝试用缓存的凭据重连（模块重启后的自动恢复）。
    pub async fn ensure_session(&self) -> Result<Session, String> {
        {
            let guard = self.session.lock().await;
            if let Some(s) = guard.as_ref() {
                return Ok(s.clone());
            }
        }
        let _connecting = self.connect_gate.lock().await;
        if let Some(session) = self.session.lock().await.as_ref() {
            return Ok(session.clone());
        }
        let cache = Cache::new(Some(&self.data_dir), None, None, None)
            .map_err(|e| format!("cache init: {e}"))?;
        let Some(creds) = cache.credentials() else {
            return Err("not logged in: call auth/login first".into());
        };
        self.connect_and_install(creds).await
    }

    /// 用给定凭据建立会话 + 播放器并接管槽位（登录成功 / 自动重连共用）。
    pub async fn connect_and_install(&self, creds: Credentials) -> Result<Session, String> {
        let cache = Cache::new(Some(&self.data_dir), None, Some(&self.data_dir), None)
            .map_err(|e| format!("cache init: {e}"))?;
        let session = Session::new(session_config(), Some(cache));
        session
            .connect(creds, true)
            .await
            .map_err(|e| format!("connect: {e}"))?;

        // Decoder/sink generations are created only for actual GET playback.
        // A session reconnect must not redirect an old decoder into a new stream.
        *self.session.lock().await = Some(session.clone());
        Ok(session)
    }

    /// song/url：校验曲目可播并预取时长，返回本地流 URL（不启动播放，
    /// 播放推迟到宿主真正发起 /stream 请求，避免对着空气解码）。
    pub async fn prepare(&self, track_id: &str) -> Result<(String, i32), String> {
        let session = self.ensure_session().await?;
        let track = fetch_track(&session, track_id).await?;
        let port = self.port.load(Ordering::SeqCst);
        let url = format!("http://127.0.0.1:{port}/stream/{track_id}");
        Ok((url, track.duration))
    }

    pub async fn track_duration(&self, track_id: &str) -> Result<i32, String> {
        let session = self.ensure_session().await?;
        Ok(fetch_track(&session, track_id).await?.duration)
    }

    /// Each decoder, sink and event receiver owns precisely one stream generation.
    pub async fn play_from(
        &self,
        track_id: &str,
        duration: i32,
        position_ms: u32,
    ) -> Result<Arc<TrackStream>, String> {
        let uri = SpotifyUri::Track {
            id: SpotifyId::from_base62(track_id).map_err(|e| format!("bad track id: {e}"))?,
        };
        // Serialize session lookup with logout; otherwise a request can capture a
        // Session, wait behind logout, then start playback on the retired session.
        let _change = self.playback_change.lock().await;
        let session = self.ensure_session().await?;
        let stream = TrackStream::create_at(&self.temp_dir(), track_id, duration, position_ms)
            .map_err(|e| format!("create stream: {e}"))?;
        // Wake a backpressured old sink BEFORE dropping Player (which joins its thread).
        if let Some(old) = self.stream_cell.lock().await.take() {
            old.cancel();
        }
        let old_player = self.player.lock().await.take();
        if let Some(old) = old_player {
            tokio::task::spawn_blocking(move || drop(old))
                .await
                .map_err(|e| format!("stop old player: {e}"))?;
        }
        let sink_stream = stream.clone();
        let player = Player::new(
            PlayerConfig {
                bitrate: Bitrate::Bitrate320,
                gapless: false,
                ..Default::default()
            },
            session,
            Box::new(NoOpVolume),
            move || -> Box<dyn librespot_playback::audio_backend::Sink> {
                Box::new(StreamSink::new(sink_stream))
            },
        );
        stream.await_decoder_position();
        let mut events = player.get_player_event_channel();
        let event_stream = stream.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    PlayerEvent::Playing { position_ms, .. } => {
                        event_stream.set_decoder_position(position_ms)
                    }
                    PlayerEvent::EndOfTrack { .. } => event_stream.finish(),
                    PlayerEvent::Stopped { .. } | PlayerEvent::Unavailable { .. } => {
                        event_stream.mark_failed()
                    }
                    _ => {}
                }
            }
            event_stream.cancel();
        });
        *self.stream_cell.lock().await = Some(stream.clone());
        player.load(uri, true, position_ms);
        *self.player.lock().await = Some(player);
        Ok(stream)
    }

    /// 设备码登录：请求 device code 并把「轮询 + 接管会话」挂到后台，
    /// 立即返回 (loginUrl, userCode) 给宿主展示。
    ///
    /// 设备码流程不需要监听端口、也不需要本机浏览器——用户在任意设备打开
    /// `loginUrl`（spotify.com/pair，若支持为 verification_uri_complete，
    /// 验证码已预填、可直接生成二维码）输入 userCode 即可。前提是 client_id
    /// 被 Spotify 启用了该流程（见本仓库 docs/device-authorization.md）。
    pub async fn auth_login_start(self: Arc<Self>) -> Result<(String, String), String> {
        let client = librespot_oauth::DeviceAuthClientBuilder::new(
            KEYMASTER_CLIENT_ID,
            LOGIN_SCOPES.to_vec(),
        )
        .build()
        .map_err(|e| format!("device auth unavailable: {e}"))?;

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(String, String), String>>();
        let inner = self.clone();
        tokio::task::spawn_blocking(move || {
            let auth = match client.request_device_code() {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Err(format!("device code: {e}")));
                    return;
                }
            };
            let pair = (auth.url().to_string(), auth.user_code().to_string());
            if tx.send(Ok(pair)).is_err() {
                return; // 调用方已超时放弃，仍继续轮询以完成登录
            }
            match client.poll_for_token(&auth) {
                Ok(token) => {
                    let core = Core::global();
                    let creds = Credentials::with_access_token(&token.access_token);
                    match core.runtime.block_on(inner.connect_and_install(creds)) {
                        Ok(_) => {
                            if core
                                .runtime
                                .block_on(inner.oauth.save_grant(token))
                                .is_err()
                            {
                                inner.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
                                log::warn!("OAuth credential persistence failed");
                                return;
                            }
                            inner.login_state.store(LOGIN_DONE, Ordering::SeqCst);
                            core.runtime.block_on(async {
                                *inner.login.lock().await = None;
                            });
                            log::info!("device login succeeded");
                        }
                        Err(e) => {
                            inner.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
                            log::warn!("connect after device login: {e}")
                        }
                    }
                }
                Err(e) => {
                    // 过期 / 被拒绝 / 网络失败：统一归为「本枚码已作废」。
                    inner.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
                    let core = Core::global();
                    core.runtime.block_on(async {
                        *inner.login.lock().await = None;
                    });
                    log::warn!("device poll: {e}")
                }
            }
        });

        match rx.await {
            Ok(Ok(pair)) => {
                *self.login.lock().await = Some(pair.clone());
                self.login_state.store(LOGIN_PENDING, Ordering::SeqCst);
                Ok(pair)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("login channel closed".into()),
        }
    }

    /// login/status（NCM 形状）：(已登录, uid, username)。
    /// Spotify username 为字符串，宿主要求数字 uid ⇒ FNV-1a 64 稳定映射。
    pub async fn login_status(&self) -> (bool, Option<i64>, Option<String>) {
        match self.ensure_session().await {
            Ok(s) => {
                let name = s.username().to_string();
                (true, Some(uid_from_username(&name)), Some(name))
            }
            Err(_) => (false, None, None),
        }
    }

    /// logout：删除缓存凭据并断开当前会话。
    pub async fn logout(&self) -> bool {
        self.oauth.clear().await;
        let _change = self.playback_change.lock().await;
        *self.session.lock().await = None;
        if let Some(stream) = self.stream_cell.lock().await.take() {
            stream.cancel();
        }
        if let Some(player) = self.player.lock().await.take() {
            let _ = tokio::task::spawn_blocking(move || drop(player)).await;
        }
        *self.login.lock().await = None;
        self.login_state.store(LOGIN_IDLE, Ordering::SeqCst);
        ["credentials", "credentials.json"]
            .iter()
            .any(|f| std::fs::remove_file(self.data_dir.join(f)).is_ok())
    }

    pub async fn current_stream_for(&self, track_id: &str) -> Option<Arc<TrackStream>> {
        // 当前实现：流以「最近一次 play_from」为准，由调用方比对曲目 id；
        // TrackStream 不保存 id，由 engine 侧的 pending 记录比对。
        let guard = self.stream_cell.lock().await;
        guard.as_ref().and_then(|s| {
            let name = s.path.file_name()?.to_string_lossy().into_owned();
            let expect = format!("cp_spotify_{track_id}_");
            name.starts_with(&expect).then(|| s.clone())
        })
    }
}

async fn fetch_track(session: &Session, track_id: &str) -> Result<Track, String> {
    let id = SpotifyId::from_base62(track_id).map_err(|e| format!("bad track id: {e}"))?;
    let uri = SpotifyUri::Track { id };
    Track::get(session, &uri)
        .await
        .map_err(|e| format!("metadata: {e}"))
}

/// 会话配置：client_id 固定为桌面 Keymaster，其余沿用默认。
fn session_config() -> SessionConfig {
    SessionConfig {
        client_id: KEYMASTER_CLIENT_ID.to_owned(),
        ..SessionConfig::default()
    }
}

/// username → 稳定数字 uid（宿主 LoginStatus 解析要求 Long）。
fn uid_from_username(name: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash & 0x7fff_ffff_ffff_ffff) as i64
}

fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CP_SPOTIFY_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // Android：应用私有目录（唯一一定可写、且随应用卸载清理的位置）。
    if let Some(files) = crate::opener::app_files_dir() {
        let p = files.join("cp_spotify");
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("user_data");
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    let p = std::env::temp_dir().join("cp_spotify_user_data");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 占位：AudioFormat 当前固定 S16（与 stream.rs 的 WAV 头一致），保留引用防误改。
const _: AudioFormat = AudioFormat::S16;
