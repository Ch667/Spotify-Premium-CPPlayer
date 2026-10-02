# librespot → CPPlayer 音源模块方案

> 分析对象：本仓库（librespot 0.8.0 fork，Rust Spotify 客户端库）
> 参照项目：`../CPPlayer-KMP`（Kotlin Multiplatform 播放器，Provider 插件系统）
> 结论先行：**可行**。librespot 是纯 Rust 库（rustls 可全静态链接、无系统依赖），具备
> Session / Player / 自定义 Sink / Token Provider / OAuth 全套能力，完全可以封装为
> CPPlayer 的 Provider 模块。核心差异点与解法见 §2、§3。

---

## 1. 可行性分析

### 1.1 librespot 侧具备的能力（已核对源码）

| 能力 | 位置 | 对接入的意义 |
|------|------|-------------|
| 会话与认证 | `core/src/session.rs`、`core/src/authentication.rs` | `Session::connect(Credentials)`，支持 access_token / 凭据缓存 |
| OAuth 登录 | `oauth/src/lib.rs`（`OAuthClientBuilder`，回环重定向 `http://127.0.0.1:8898/login`） | 首次登录可直接拉起浏览器，无需内嵌网页 |
| Token Provider | `session.token_provider().get_token(scopes)`（见 `examples/get_token.rs`） | 拿到带 scope 的 token 后可调用 **Spotify Web API** 补齐搜索/歌单等 |
| 播放内核 | `playback/src/player.rs`（`Player::new / load / preload / play / pause / seek`） | 库形态嵌入，完全可控 |
| 自定义音频输出 | `playback/src/audio_backend/mod.rs` 的 `Sink` trait（`start/stop/write(AudioPacket, Converter)`） | **关键**：实现一个"写内存环"的 Sink，即可把 PCM 交给本地 HTTP 流 |
| 元数据 | `metadata/`（Mercury 协议取曲目/专辑/艺人） | `song/detail` 可直接用 |
| 纯 Rust TLS | feature `rustls-tls-webpki-roots` | 交叉编译 Android/桌面零系统依赖 |

### 1.2 唯一的结构性差异

网易云等音源能返回一个可直接 GET 的 CDN URL；**Spotify 不存在这种 URL** —— 音频
必须先建立会话、换取 audio key，再从加密 CDN 拉流解密解码。因此：

> **解法：模块内嵌一个本地 HTTP 流服务器（axum），`song/url/v1` 返回
> `http://127.0.0.1:{port}/stream/{trackId}`。** CPPlayer（Media3/ExoPlayer）
> 拉这个地址即可播放，WAV 封装使其无需任何解码配置。这正是参考模块
> `netease-module-rust`（axum + JNI + 本地 server）的既有模式，宿主端零改造。

### 1.3 能力缺口（必须如实面对）

| Provider 标准 API | 实现路径 | 状态 |
|------|------|------|
| `cloudsearch` | Web API `/v1/search`（token 来自 token_provider） | ✅ 可做 |
| `song/url/v1`(+`/302`) | 本地 WAV 流（见 §2） | ✅ 可做 |
| `song/detail` | `librespot-metadata`（Mercury）或 Web API | ✅ 可做 |
| 歌单 / 专辑 / 艺人 / 收藏 | Web API（`playlist-read-private` 等 scope） | ✅ 可做 |
| `lyric/new` | **Web API 不提供同步歌词**，逐字歌词无来源 | ⚠️ 只能返回空/普通文本，属硬缺口 |
| 搜索建议、电台、MV | Spotify 无对应概念或 Web API 未开放 | ⚠️ 按指南 §8 返回不支持 |

其他硬约束：**需要 Spotify Premium 账号**（librespot 免费账号无法播放）；同一账号
与官方 App/其他 Connect 设备会互踢；使用 Spotify 私有协议存在 ToS 风险，模块应
声明为个人使用。

---

## 2. 总体架构

```
CPPlayer (JVM, Android/Desktop)
   │  JNI: nativeCallApi(method, paramsJson)
   │       startNativeServer(host, port)
   ▼
cp-spotify-module (Rust cdylib, 本方案新增 crate)
   ├─ JNI 导出层 ──▶ axum 本地服务 (127.0.0.1:{port})
   │      ├─ POST /api/{method}      Provider 标准 JSON API
   │      └─ GET  /stream/{trackId}  WAV 音频流
   ├─ SpotifyEngine（单例）
   │      ├─ Session + OAuth 凭据（缓存于模块 user_data/，更新时保留）
   │      ├─ Player + StreamSink（PCM 环形缓冲 → HTTP 响应）
   │      └─ Player::preload() 预取下一首，掩盖首包延迟
   └─ WebApi 客户端（token_provider 取 token → search/playlist/album）
```

关键流程：CPPlayer 请求 `song/url/v1` → 模块立即返回本地流地址（<10ms）→
CPPlayer 的播放器开始拉流 → 引擎此时才 `Player::load(track)`，key 交换与 CDN
首包延迟（约 1–2s）被缓冲掩盖；切歌时 `preload` 提前预热。

WAV 参数建议：`AudioFormat::S16`、44.1kHz、双声道，回环带宽约 1.4 Mbps，无压力。

---

## 3. 模块形态选型

采用 **JNI Provider**（与参考模块 `netease-module-rust` 完全同构，宿主已有成熟支持）：

| 对比项 | JNI（选定） | Binary | HTTP |
|--------|------------|--------|------|
| Android 可用 | ✅ | ❌（exec 受限） | ❌（要用户自管进程） |
| Desktop 可用 | ✅（dll/dylib） | ✅ | ✅ |
| 生命周期管理 | 随宿主进程 | 子进程，需重启逻辑 | 用户自管 |
| 先例 | 参考模块已验证 | 无 | 无 |

同 crate 提供 `bin/server.rs`（独立可执行 `--port` 模式）用于调试和 headless 场景，
不影响主形态。

JNI 导出（符号前缀为宿主新包名 `cp.player.core`，见指南 v3.0.0 §3.3）：

```c
Java_cp_player_core_provider_JniProvider_nativeCallApi(env, obj, method, paramsJson)
Java_cp_player_core_provider_JniProvider_startNativeServer(env, obj, host, port)
```

---

## 4. 工程结构（本仓库新增 workspace 成员）

```
Spotify-Premium-CPPlayer/
├── cpmodule/                        # 新增 crate（workspace member）
│   ├── Cargo.toml                   # name="cp-spotify-module", version 单一来源
│   └── src/
│       ├── lib.rs                   # JNI 导出 + 模块生命周期
│       ├── engine.rs                # SpotifyEngine：Session/Player 管理
│       ├── stream.rs                # StreamSink（PCM 环形缓冲）+ WAV 封装
│       ├── api/                     # Provider 标准 API 实现
│       │   ├── mod.rs, search.rs, song_url.rs, song_detail.rs,
│       │   ├── lyric.rs, playlist.rs, album.rs, auth.rs
│       └── bin/server.rs            # 可选独立调试入口
└── scripts/                         # 多架构构建与打包脚本
    ├── build_android.sh             # cargo-ndk 三 ABI
    ├── build_desktop.ps1 / .sh      # win/mac/linux
    └── pack_module.py               # 生成 manifest.json + 打 zip
```

要点：

- `cpmodule` 通过 `path` 依赖本仓库 `librespot-core/-playback/-metadata/-oauth`，
  **版本天然锁定，不存在 crates.io 漂移**。
- TLS 统一 `rustls-tls-webpki-roots`；禁用默认 `native-tls`（Android 交叉编译会
  卡 OpenSSL）。
- 依赖增补：`axum`、`jni 0.21`、`tokio`、`serde_json`、`tracing-android`（可选）。
- feature 划分仿参考模块：`jni` / `jni-android` / `server`，按目标启用。

---

## 5. 三端多架构构建矩阵

| 端 | Rust target | 产物 | 工具链 |
|----|-------------|------|--------|
| Android | `aarch64-linux-android`（arm64-v8a） | `lib/{abi}/libcp_spotify.so` | cargo-ndk |
| Android | `armv7-linux-androideabi`（armeabi-v7a） | 同上 | cargo-ndk |
| Android | `x86_64-linux-android` | 同上 | cargo-ndk |
| Windows | `x86_64-pc-windows-msvc` | `cp_spotify.dll` | 本机 MSVC |
| macOS | `aarch64-apple-darwin` + `x86_64-apple-darwin` | `libcp_spotify.dylib` | 本机 + `-target` 交叉 |
| Linux | `x86_64-unknown-linux-gnu`（可选 aarch64-musl） | `libcp_spotify.so` | 本机 / cross |

Android 构建必须带（参考模块同款）：`RUSTFLAGS="-Clink-arg=-Wl,-z,max-page-size=16384"`。

打包遵循指南 §10：`manifest.json`（`type:"jni"`、`entryPoint`、`supportedAbis`、
`apiMap`）+ `lib/{abi}/*.so`；桌面端按参考模块 `build_module_desktop.ps1` 的目录
约定。apiMap 最小集：

```json
{
  "cloudsearch": "search",
  "song/url/v1": "song/url",
  "song/url/v1/302": "song/url",
  "song/detail": "song/detail",
  "lyric/new": "lyric"
}
```

---

## 6. 自动版本控制与发版

### 6.1 版本来源（单一事实源）

- 模块版本唯一来源：`cpmodule/Cargo.toml` 的 `version`（semver）。
- 与宿主 CPPlayer 的 `gradle.properties` 体系**解耦**，各自独立发版。
- 若宿主校验数字版本，按宿主同款公式派生：`major*1_000_000 + minor*1_000 + patch`。

### 6.2 发布流水线（对齐宿主 `release.ps1` 的纪律）

1. **本地脚本 `scripts/release_module.ps1`**：semver 校验 → 强制干净工作树 →
   查重本地/远端 tag → 打 tag `module-v{X.Y.Z}` 并推送。
2. **GitHub Actions（tag 触发）matrix 构建**：
   - `ubuntu-latest`：cargo-ndk 三 ABI（配 NDK 缓存）
   - `windows-latest`：msvc dll
   - `macos-latest`：两个 darwin target
3. **汇聚打包**：job 汇总各端产物 → `pack_module.py` 用 tag 注入 manifest.json 的
   `version` → 分端产出 `cp-spotify-module-{ver}-{android|windows|macos|linux}.zip`
   （或按宿主导入规则合包）→ 上传 GitHub Release。
4. **Changelog 自动生成**：约定式提交 + git-cliff → Release notes。
5. **模块内自动更新**（指南 §10.2 契约）：Release workflow 同时生成
   `version.json`（`{version, downloadUrl, changelog}`，downloadUrl 指向本次
   Release zip），manifest.json 的 `updateUrl` 指向它的固定 raw 地址。宿主检查
   更新时 `user_data/` 保留 → **凭据与用户配置天然跨版本存活**。

---

## 7. 里程碑

| 阶段 | 内容 | 验收 |
|------|------|------|
| M1 桌面 spike | crate 骨架 + OAuth 登录 + 本地 WAV 流 + search/song-url/song-detail | Desktop 导入 zip，能搜能播 |
| M2 JNI + Android | JNI 导出层、Android 三 ABI、凭据存 `user_data/` | app-android 内导入可播 |
| M3 CI 发版 | Actions matrix + 自动打包 + updateUrl 更新链路 | 推 tag 出全量 zip |
| M4 补全 | 歌单/专辑/收藏、PCM 缓存、preload 队列、错误码对齐指南 §8 | 全 API 冒烟 |

## 8. 风险清单

1. **Premium 硬门槛**；免费账号直接不可用，需在模块 API 层给出明确错误提示。
2. **歌词缺口**：无逐字歌词来源，`lyric/new` 返回空，UI 端需容错。
3. **首包延迟**：靠"先回 URL 后取流 + preload"掩盖；极端弱网下 seek 起始会卡顿。
4. **会话互踢**：同账号在官方 App 与本模块间会互踢，属 Spotify Connect 机制。
5. **ToS/法律**：私有协议客户端仅建议个人使用，模块分发页需声明。
6. **JNI 同步调用**：`nativeCallApi` 在宿主线程同步执行，搜索类请求必须带缓存与
   超时（建议 ≤3s），避免 Android ANR。
