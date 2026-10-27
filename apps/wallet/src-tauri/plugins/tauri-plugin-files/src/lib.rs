//! Save or share a file the wallet made (multisig wallet exports, PSBTs).
//!
//! Android: "Share" opens the share sheet (send it to Signal, Drive, Files…);
//! "Save" opens the system save-as screen. Desktop: saved to the Downloads
//! folder (share does the same).

use serde::Deserialize;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("files")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api.register_android_plugin("io.btcsessions.files", "FilesPlugin")?;
                app.manage(Files::<R> { handle });
            }
            #[cfg(not(target_os = "android"))]
            app.manage(Files::<R> {
                app: app.clone(),
            });
            Ok(())
        })
        .build()
}

pub struct Files<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    app: tauri::AppHandle<R>,
}

#[derive(Deserialize)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct Done {
    done: bool,
}

/// What happened: `Ok(true)` saved or shared, `Ok(false)` the user backed
/// out, `Err` with a plain message.
pub type Outcome = Result<bool, String>;

#[cfg(target_os = "android")]
impl<R: Runtime> Files<R> {
    async fn call(&self, method: &str, filename: &str, mime: &str, content: &str) -> Outcome {
        self.handle
            .run_mobile_plugin_async::<Done>(
                method,
                serde_json::json!({ "filename": filename, "mime": mime, "content": content }),
            )
            .await
            .map(|d| d.done)
            .map_err(|e| match e {
                tauri::plugin::mobile::PluginInvokeError::InvokeRejected(r) => {
                    r.message.unwrap_or_else(|| "Couldn't save the file".into())
                }
                other => other.to_string(),
            })
    }

    pub async fn share(&self, filename: &str, mime: &str, content: &str) -> Outcome {
        self.call("share", filename, mime, content).await
    }

    pub async fn save(&self, filename: &str, mime: &str, content: &str) -> Outcome {
        self.call("save", filename, mime, content).await
    }
}

#[cfg(not(target_os = "android"))]
impl<R: Runtime> Files<R> {
    /// Desktop: into Downloads (a new name if one exists). Returns the path
    /// in the error-free case through `saved_to`.
    pub async fn save(&self, filename: &str, _mime: &str, content: &str) -> Outcome {
        let dir = self
            .app
            .path()
            .download_dir()
            .map_err(|e| format!("No Downloads folder: {e}"))?;
        let safe: String = filename
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "._- ".contains(c) { c } else { '_' })
            .collect();
        let mut path = dir.join(&safe);
        let mut n = 1;
        while path.exists() {
            let (stem, ext) = safe.rsplit_once('.').unwrap_or((safe.as_str(), ""));
            path = dir.join(if ext.is_empty() {
                format!("{stem} ({n})")
            } else {
                format!("{stem} ({n}).{ext}")
            });
            n += 1;
        }
        std::fs::write(&path, content).map_err(|e| format!("Couldn't save to {}: {e}", path.display()))?;
        Ok(true)
    }

    pub async fn share(&self, filename: &str, mime: &str, content: &str) -> Outcome {
        self.save(filename, mime, content).await
    }
}
