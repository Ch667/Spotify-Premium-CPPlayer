//! JNI 导出：对接 CPPlayer `cp.player.core.provider.JniProvider`。
//!
//! 必须导出（Provider 指南 §3.3，符号前缀随宿主包名 `cp.player.core`）：
//! ```c
//! jstring nativeCallApi(JNIEnv*, jobject, jstring method, jstring paramsJson);
//! void    startNativeServer(JNIEnv*, jobject, jstring host, jint port);
//! ```
//! 两个导出都在宿主调用线程同步执行：
//! - `nativeCallApi` 内部对请求做 `block_on`，搜索类请求自带缓存/超时要求由
//!   实现保证（Web API 单次请求，无重试风暴）。
//! - `startNativeServer` 绑定端口后立即返回（服务已在后台 accept）。

use std::panic::{catch_unwind, AssertUnwindSafe};

use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use serde_json::json;

use crate::api;
use crate::engine::Core;

fn jstr_to_string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s)
        .ok()
        .map(|o| o.to_string_lossy().into_owned())
}

fn err_response(env: &mut JNIEnv, msg: &str) -> jstring {
    let v = json!({"code": 500, "msg": msg});
    env.new_string(v.to_string())
        .map(JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// API 调用（核心）。返回 JSON 字符串（宿主负责释放 local ref）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_cp_player_core_provider_JniProvider_nativeCallApi(
    mut env: JNIEnv,
    _obj: JClass,
    method: JString,
    params: JString,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(method) = jstr_to_string(&mut env, &method) else {
            return err_response(&mut env, "missing method");
        };
        let params_str = jstr_to_string(&mut env, &params).unwrap_or_else(|| "{}".into());
        let params: serde_json::Value =
            serde_json::from_str(&params_str).unwrap_or(json!({}));

        let core = Core::global();
        let resp = core.runtime.block_on(api::dispatch(core, &method, params));
        env.new_string(resp.to_string())
            .map(JString::into_raw)
            .unwrap_or(std::ptr::null_mut())
    }));
    result.unwrap_or_else(|_| err_response(&mut env, "panic in nativeCallApi"))
}

/// 启动本地服务（指南：可选导出）。绑定成功即返回。
#[unsafe(no_mangle)]
pub extern "system" fn Java_cp_player_core_provider_JniProvider_startNativeServer(
    mut env: JNIEnv,
    _obj: JClass,
    host: JString,
    port: jint,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // v1 固定回环监听（音频流与 API 均不出本机）；host 参数仅为契约兼容。
        let _ = jstr_to_string(&mut env, &host);
        let port = if port > 0 { port as u16 } else { 0 };
        let core = Core::global();
        match core.runtime.block_on(api::serve(port)) {
            Ok(bound) => log::info!("local server on 127.0.0.1:{bound}"),
            Err(e) => log::error!("start server: {e}"),
        }
    }));
}
