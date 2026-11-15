//! 设备码登录的浏览器跳转。
//!
//! Spotify 的设备码授权页是**网页**（`https://spotify.com/pair?code=XXXXXX`），
//! 不是 Spotify App 能识别的二维码码，所以宿主「用音源对应的 App 扫码」那条路走不通。
//!
//! 这里在模块内部用 JNI 直接 `startActivity(ACTION_VIEW)` 调起系统浏览器，
//! 用户点「扫码登录」后浏览器自动打开授权页；桌面端没有 JavaVM，调用直接跳过。

#[cfg(feature = "jni-core")]
use std::sync::OnceLock;

#[cfg(feature = "jni-core")]
use jni::objects::{JObject, JValue};
#[cfg(feature = "jni-core")]
use jni::JavaVM;

#[cfg(feature = "jni-core")]
static VM: OnceLock<JavaVM> = OnceLock::new();

/// 记录宿主的 JavaVM（JNI 入口每次调用都设置，重复设置无副作用）。
#[cfg(feature = "jni-core")]
pub fn attach_vm(vm: JavaVM) {
    let _ = VM.set(vm);
}

/// 用系统浏览器打开 `url`。返回是否真的发出了 Intent。
///
/// - Android（有 JavaVM）：`ActivityThread.currentApplication()` 拿 Application，
///   补上 `FLAG_ACTIVITY_NEW_TASK` 后 `startActivity`。
/// - 其他平台 / 拿不到上下文：返回 `false`，不影响原流程。
pub fn open_url(url: &str) -> bool {
    #[cfg(feature = "jni-core")]
    {
        let Some(vm) = VM.get() else {
            log::debug!("open_url: no JavaVM (desktop), skip browser launch");
            return false;
        };
        match open_with_vm(vm, url) {
            Ok(()) => {
                log::info!("open_url: browser launched for device code page");
                true
            }
            Err(e) => {
                log::warn!("open_url failed: {e}");
                false
            }
        }
    }
    #[cfg(not(feature = "jni-core"))]
    {
        let _ = url;
        false
    }
}

#[cfg(feature = "jni-core")]
fn open_with_vm(vm: &JavaVM, url: &str) -> Result<(), jni::errors::Error> {
    let mut env = vm.attach_current_thread()?;

    // 应用上下文：Application 也是 Context，只是 startActivity 必须带 NEW_TASK。
    let activity_thread = env.find_class("android/app/ActivityThread")?;
    let app: JObject = env
        .call_static_method(
            &activity_thread,
            "currentApplication",
            "()Landroid/app/Application;",
            &[],
        )?
        .l()?;

    let intent_class = env.find_class("android/content/Intent")?;
    let intent = env.new_object(&intent_class, "()V", &[])?;

    let action = env.new_string("android.intent.action.VIEW")?;
    env.call_method(
        &intent,
        "setAction",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&action)],
    )?;

    let uri_class = env.find_class("android/net/Uri")?;
    let url_str = env.new_string(url)?;
    let uri: JObject = env
        .call_static_method(
            &uri_class,
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[JValue::Object(&url_str)],
        )?
        .l()?;
    env.call_method(
        &intent,
        "setData",
        "(Landroid/net/Uri;)Landroid/content/Intent;",
        &[JValue::Object(&uri)],
    )?;

    // Intent.FLAG_ACTIVITY_NEW_TASK
    env.call_method(
        &intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[JValue::Int(0x1000_0000)],
    )?;

    env.call_method(
        &app,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[JValue::Object(&intent)],
    )?;

    // 有意保留：本地引用由 JNI 帧回收，模块生命周期内调用次数极少。
    Ok(())
}

/// 应用私有目录 `Context.getFilesDir()`（Android）。没有 JavaVM（桌面）或取不到时返回 `None`。
///
/// Android 上当前目录是 `/`、系统临时目录是 `/data/local/tmp`，应用都写不进去：
/// 凭据缓存存不下来（每次重启都要重新登录），WAV 临时流文件也建不出来。
#[cfg(feature = "jni-core")]
pub fn app_files_dir() -> Option<std::path::PathBuf> {
    let vm = VM.get()?;
    let mut env = vm.attach_current_thread().ok()?;
    let at = env.find_class("android/app/ActivityThread").ok()?;
    let app: JObject = env
        .call_static_method(&at, "currentApplication", "()Landroid/app/Application;", &[])
        .ok()?
        .l()
        .ok()?;
    if app.is_null() {
        return None;
    }
    let dir: JObject = env.call_method(&app, "getFilesDir", "()Ljava/io/File;", &[]).ok()?.l().ok()?;
    if dir.is_null() {
        return None;
    }
    let path: JObject = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = jni::objects::JString::from(path);
    let out: String = env.get_string(&path).ok()?.into();
    Some(std::path::PathBuf::from(out))
}

#[cfg(not(feature = "jni-core"))]
pub fn app_files_dir() -> Option<std::path::PathBuf> {
    None
}

/// 库被 JVM 加载时记下 `JavaVM`。
///
/// 新协议（kmp-sdk-v1）的入口不再经过旧的 `nativeCallApi`，没有地方能顺手 `attach_vm`，
/// 所以在 `JNI_OnLoad` 里取。SDK 本身不导出这个符号，不会冲突。
#[cfg(all(feature = "jni-core", feature = "sdk"))]
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut std::ffi::c_void) -> jni::sys::jint {
    attach_vm(vm);
    jni::sys::JNI_VERSION_1_6
}
