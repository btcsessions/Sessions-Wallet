//! Payment alerts.
//!
//! On Android:
//! * system notifications for payments (posted through JNI with the
//!   application context, so they work after the app's screen is gone);
//! * a foreground service ("Watching for payments") that keeps the app's
//!   process, and so the wallet's live chain and Lightning connections,
//!   running after the app is swiped away;
//! * the notification permission (Android 13+) and the "unrestricted
//!   battery" exemption, which keeps the network up while the phone dozes.
//!
//! Elsewhere every call is a harmless no-op and [`Status::supported`] is
//! false.

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

/// Register with `tauri::Builder::plugin`; then `app.state::<Alerts<R>>()`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("alerts")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle =
                    _api.register_android_plugin("io.btcsessions.alerts", "AlertsPlugin")?;
                app.manage(Alerts::<R> { handle });
            }
            #[cfg(not(target_os = "android"))]
            app.manage(Alerts::<R> {
                _r: std::marker::PhantomData,
            });
            Ok(())
        })
        .build()
}

/// Managed state: permission and settings prompts (these need the app's
/// screen). Posting and the watch service are the free functions below.
pub struct Alerts<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    _r: std::marker::PhantomData<fn() -> R>,
}

/// Whether notifications can be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Granted,
    /// Not yet asked (Android 13+): asking shows the system prompt.
    Prompt,
    /// Refused, or switched off in system settings: only the settings page
    /// can turn it back on.
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Payment alerts exist on this platform.
    pub supported: bool,
    pub notifications: Permission,
    /// The watch service is running.
    pub watching: bool,
    /// The app may use the network while the phone dozes ("Unrestricted"
    /// battery use).
    pub unrestricted: bool,
}

#[cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]
impl Status {
    fn unsupported() -> Self {
        Status {
            supported: false,
            notifications: Permission::Denied,
            watching: false,
            unrestricted: false,
        }
    }
}

#[cfg(target_os = "android")]
impl<R: Runtime> Alerts<R> {
    pub async fn status(&self) -> Status {
        self.handle
            .run_mobile_plugin_async::<Status>("status", serde_json::json!({}))
            .await
            .unwrap_or_else(|e| {
                eprintln!("alerts: status: {e}");
                Status::unsupported()
            })
    }

    /// The system prompt (Android 13+), or the app's notification settings
    /// when it can't be shown. Resolves with the new status.
    pub async fn request_notifications(&self) -> Status {
        match self
            .handle
            .run_mobile_plugin_async::<Status>("requestNotifications", serde_json::json!({}))
            .await
        {
            Ok(s) => s,
            Err(_) => self.status().await,
        }
    }

    /// Ask to be exempt from battery optimisation (a system dialog).
    pub async fn request_unrestricted(&self) -> Status {
        match self
            .handle
            .run_mobile_plugin_async::<Status>("requestUnrestricted", serde_json::json!({}))
            .await
        {
            Ok(s) => s,
            Err(_) => self.status().await,
        }
    }
}

/// Desktop: system notifications, and "watching" keeps the app running in
/// the menu bar / system tray when its window is closed.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl<R: Runtime> Alerts<R> {
    pub async fn status(&self) -> Status {
        Status {
            supported: true,
            // macOS asks the first time a notification is shown; Linux and
            // Windows don't ask.
            notifications: Permission::Granted,
            watching: desktop::watching(),
            unrestricted: true,
        }
    }
    pub async fn request_notifications(&self) -> Status {
        self.status().await
    }
    pub async fn request_unrestricted(&self) -> Status {
        self.status().await
    }
}

#[cfg(target_os = "ios")]
impl<R: Runtime> Alerts<R> {
    pub async fn status(&self) -> Status {
        Status::unsupported()
    }
    pub async fn request_notifications(&self) -> Status {
        Status::unsupported()
    }
    pub async fn request_unrestricted(&self) -> Status {
        Status::unsupported()
    }
}

/// Desktop state the app's window code reads: whether to keep running when
/// the window closes, and whether the window has focus (then the app shows
/// its own in-window alert instead of a system notification).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop {
    use std::sync::atomic::{AtomicBool, Ordering};

    static WATCHING: AtomicBool = AtomicBool::new(false);
    static FOCUSED: AtomicBool = AtomicBool::new(true);

    pub fn watching() -> bool {
        WATCHING.load(Ordering::SeqCst)
    }

    pub(crate) fn set_watching(on: bool) {
        WATCHING.store(on, Ordering::SeqCst);
    }

    /// Call from the window's focus events.
    pub fn set_focused(on: bool) {
        FOCUSED.store(on, Ordering::SeqCst);
    }

    pub fn focused() -> bool {
        FOCUSED.load(Ordering::SeqCst)
    }

    pub(crate) fn show(title: &str, body: &str) -> bool {
        let mut n = notify_rust::Notification::new();
        n.summary(title).body(body).appname("Sessions Wallet");
        #[cfg(target_os = "linux")]
        n.icon("io.btcsessions.wallet");
        match n.show() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("alerts: notification failed: {e}");
                false
            }
        }
    }
}

/// One payment notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice<'a> {
    /// Same key → replaces the earlier notification (e.g. "confirmed"
    /// replacing "incoming").
    pub key: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// What the lock screen shows instead (no amounts).
    pub public_title: &'a str,
}

/// Show a payment notification, unless the app is on screen (the app shows
/// its own). Returns whether one was posted.
pub fn post(notice: &Notice<'_>) -> bool {
    #[cfg(target_os = "android")]
    {
        match android::post(notice) {
            Ok(posted) => posted,
            Err(e) => {
                eprintln!("alerts: post failed: {e}");
                false
            }
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if desktop::focused() {
            return false;
        }
        desktop::show(notice.title, notice.body)
    }
    #[cfg(target_os = "ios")]
    {
        let _ = notice;
        false
    }
}

/// Start the watch service (no-op if running). Call while the app is on
/// screen: Android doesn't let a background app start one.
pub fn start_watch() {
    #[cfg(target_os = "android")]
    if let Err(e) = android::call_void("startWatch") {
        eprintln!("alerts: start watch failed: {e}");
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    desktop::set_watching(true);
}

/// Stop the watch service (no-op if stopped).
pub fn stop_watch() {
    #[cfg(target_os = "android")]
    if let Err(e) = android::call_void("stopWatch") {
        eprintln!("alerts: stop watch failed: {e}");
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    desktop::set_watching(false);
}

/// The app's screen is showing right now.
pub fn in_foreground() -> bool {
    #[cfg(target_os = "android")]
    {
        android::call_bool("inForeground").unwrap_or(false)
    }
    #[cfg(not(target_os = "android"))]
    {
        #[cfg(not(target_os = "ios"))]
        return desktop::focused();
        #[cfg(target_os = "ios")]
        true
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::Notice;
    use jni::objects::{JClass, JObject, JValue};
    use jni::{JNIEnv, JavaVM};

    const CLASS: &str = "io.btcsessions.alerts.Alerts";

    /// Run `f` with a JNI env and the application context. Clears any Java
    /// exception so it can't leak into the next call.
    fn with_env<T>(
        f: impl for<'a> FnOnce(&mut JNIEnv<'a>, &JObject<'a>, JClass<'a>) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        let ctx = ndk_context::android_context();
        if ctx.vm().is_null() || ctx.context().is_null() {
            return Err("no Android context yet".into());
        }
        // SAFETY: tao initialised ndk_context with the process's JavaVM and a
        // global ref to the application context, both valid for the life of
        // the process.
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let context = unsafe { JObject::from_raw(ctx.context().cast()) };
        let r = (|| {
            // A native thread's FindClass only sees system classes; the
            // app's own come from the context's class loader.
            let loader = env
                .call_method(&context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
                .l()?;
            let name = env.new_string(CLASS)?;
            let class = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&name)],
                )?
                .l()?;
            f(&mut env, &context, JClass::from(class))
        })();
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        // The context is a global ref owned by ndk_context: never delete it.
        std::mem::forget(context);
        r.map_err(|e| e.to_string())
    }

    pub(super) fn post(n: &Notice<'_>) -> Result<bool, String> {
        with_env(|env, ctx, class| {
            let key = env.new_string(n.key)?;
            let title = env.new_string(n.title)?;
            let body = env.new_string(n.body)?;
            let public = env.new_string(n.public_title)?;
            env.call_static_method(
                class,
                "post",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Z",
                &[
                    JValue::Object(ctx),
                    JValue::Object(&key),
                    JValue::Object(&title),
                    JValue::Object(&body),
                    JValue::Object(&public),
                ],
            )?
            .z()
        })
    }

    pub(super) fn call_void(method: &str) -> Result<(), String> {
        with_env(|env, ctx, class| {
            env.call_static_method(
                class,
                method,
                "(Landroid/content/Context;)V",
                &[JValue::Object(ctx)],
            )?;
            Ok(())
        })
    }

    pub(super) fn call_bool(method: &str) -> Result<bool, String> {
        with_env(|env, _ctx, class| env.call_static_method(class, method, "()Z", &[])?.z())
    }
}
