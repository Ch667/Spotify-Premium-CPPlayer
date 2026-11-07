//! cp-spotify-module —— CPPlayer 音源模块（JNI Provider）。
//!
//! 把 librespot 封装为 CPPlayer Provider 契约（docs/dev/PROVIDER_DEV_GUIDE.md）：
//! - JNI 导出：`Java_cp_player_core_provider_JniProvider_nativeCallApi` /
//!   `startNativeServer`（feature `jni`）
//! - 本地服务：`POST /api/{method}`（Provider 标准 JSON API）、
//!   `GET /stream/{trackId}`（WAV 流）、`GET /health`
//! - 音频路径：librespot Player → StreamSink → 本地 WAV 流
//! - 登录：Spotify 设备码流程（spotify.com/pair），凭据缓存于模块 user_data/
//!
//! 需 Spotify Premium 账号；同一账号与官方 App 会互踢（Connect 机制）。

pub mod api;
pub mod engine;
pub mod opener;
pub mod stream;

#[cfg(feature = "jni")]
pub mod jni;
