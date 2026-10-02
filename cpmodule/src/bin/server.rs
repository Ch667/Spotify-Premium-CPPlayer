//! 独立调试服务：桌面端等价于 JNI 模块的行为，便于用 curl 冒烟。
//!
//! ```text
//! cargo run -p cp-spotify-module --bin cp-spotify-server --features server -- --port 8899
//! curl -X POST http://127.0.0.1:8899/api/auth/login -d '{}'
//! ```

use cp_spotify::{api, engine::Core};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut port: u16 = 8899;
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--port") {
        if let Some(v) = args.get(pos + 1) {
            port = v.parse().unwrap_or(port);
        }
    }

    let core = Core::global();
    let bound = core
        .runtime
        .block_on(api::serve(port))
        .expect("bind server");

    eprintln!(
        "cp-spotify-server listening on http://127.0.0.1:{bound}  (data dir: {:?})",
        core.inner.data_dir
    );
    core.runtime.block_on(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("shutting down");
        }
    });
}
