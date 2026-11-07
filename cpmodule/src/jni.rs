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
//!
//! 登录体验补丁（v0.1.1）：Spotify 设备码流程返回的是**网页链接**
//! （`https://spotify.com/pair?code=XXXXXX`），宿主却提示「用音源对应的 App 扫码」，
//! 而 Spotify App 只认 `spotify:` 开头的码，扫网页链接没有任何反应。
//! 这里在 `login/qr/create` 返回后直接 `ACTION_VIEW` 调起系统浏览器，
//! 用户点一下登录就跳到网页授权，不必自己扫码。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, Ordering};

use jni::objects::{JClass, JString};
#[cfg(target_os = "android")]
use jni::objects::{JObject, JValue, JValueOwned};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use serde_json::{json, Value};

use crate::api;
use crate::engine::Core;

/// 同一枚链接的去重窗口（毫秒）：宿主进账户页 + 手动刷新可能连续触发 create。
const OPEN_DEDUP_MS: i64 = 2500;
static LAST_OPEN_MS: AtomicI64 = AtomicI64::new(0);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 用系统浏览器打开链接。
///
/// Context 取自 `ActivityThread.currentActivityThread().getApplication()`，
/// 再以 `ACTION_VIEW` + `FLAG_ACTIVITY_NEW_TASK` 起浏览器（Application 不是 Activity，
/// 必须带这个 flag）。任何一步失败只记日志，不影响接口本身：宿主仍会照常显示二维码。
#[cfg(target_os = "android")]
fn open_in_browser(env: &mut JNIEnv, url: &str) -> Result<(), String> {
    let activity_thread = env
        .find_class("android/app/ActivityThread")
        .map_err(|e| format!("find ActivityThread: {e}"))?;
    let thread = env
        .call_static_method(
            activity_thread,
            "currentActivityThread",
            "()Landroid/app/ActivityThread;",
            &[],
        )
        .map_err(|e| format!("currentActivityThread: {e}"))?;
    let thread = match thread {
        JValueOwned::Object(o) => o,
        _ => return Err("currentActivityThread returned non-object".into()),
    };
    let app = env
        .call_method(&thread, "getApplication", "()Landroid/app/Application;", &[])
        .map_err(|e| format!("getApplication: {e}"))?;
    let app = match app {
        JValueOwned::Object(o) => o,
        _ => return Err("Application is null".into()),
    };

    let action = env
        .new_string("android.intent.action.VIEW")
        .map_err(|e| format!("new_string action: {e}"))?;
    let action_obj = JObject::from(action);
    let intent = env
        .new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&action_obj)],
        )
        .map_err(|e| format!("new Intent: {e}"))?;

    let url_js = env
        .new_string(url)
        .map_err(|e| format!("new_string url: {e}"))?;
    let url_obj = JObject::from(url_js);
    let uri = env
        .call_static_method(
            "android/net/Uri",
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[JValue::Object(&url_obj)],
        )
        .map_err(|e| format!("Uri.parse: {e}"))?;
    let uri = match uri {
        JValueOwned::Object(o) => o,
        _ => return Err("Uri.parse returned non-object".into()),
    };

    env.call_method(
        &intent,
        "setData",
        "(Landroid/net/Uri;)Landroid/content/Intent;",
        &[JValue::Object(&uri)],
    )
    .map_err(|e| format!("setData: {e}"))?;
    // FLAG_ACTIVITY_NEW_TASK
    env.call_method(
        &intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[JValue::Int(0x1000_0000)],
    )
    .map_err(|e| format!("addFlags: {e}"))?;
    env.call_method(
        &app,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[JValue::Object(&intent)],
    )
    .map_err(|e| format!("startActivity: {e}"))?;

    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
        return Err("startActivity threw".into());
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn open_in_browser(_env: &mut JNIEnv, _url: &str) -> Result<(), String> {
    Ok(())
}

/// 从接口返回里取要丢给浏览器的地址（设备码流程：`data.qrurl`；调试通道：`data.loginUrl`）。
fn pick_url(resp: &Value) -> Option<&str> {
    resp.pointer("/data/qrurl")
        .and_then(Value::as_str)
        .or_else(|| resp.pointer("/data/loginUrl").and_then(Value::as_str))
        .filter(|u| u.starts_with("https://"))
}

/// 这条接口的返回是否需要顺手打开浏览器。
fn should_auto_open(method: &str) -> bool {
    matches!(method, "login/qr/create" | "qr/create" | "auth/login")
}

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
        // 记下 JavaVM，供设备码登录时调起系统浏览器（opener::open_url）。
        if let Ok(vm) = env.get_java_vm() {
            crate::opener::attach_vm(vm);
        }

        let params_str = jstr_to_string(&mut env, &params).unwrap_or_else(|| "{}".into());
        let params: serde_json::Value =
            serde_json::from_str(&params_str).unwrap_or(json!({}));

        let core = Core::global();
        let resp = core.runtime.block_on(api::dispatch(core, &method, params));

        // 登录类接口：把设备码链接直接丢给系统浏览器。
        // 失败不影响返回值——宿主照样展示二维码，用户仍可手动扫码/复制链接。
        if should_auto_open(&method) {
            if let Some(url) = pick_url(&resp) {
                let ts = now_ms();
                let last = LAST_OPEN_MS.load(Ordering::SeqCst);
                if ts - last > OPEN_DEDUP_MS {
                    LAST_OPEN_MS.store(ts, Ordering::SeqCst);
                    let url = url.to_string();
                    if let Err(e) = open_in_browser(&mut env, &url) {
                        log::warn!("auto open browser failed ({e}); url = {url}");
                    } else {
                        log::info!("auto opened browser: {url}");
                    }
                }
            }
        }

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
