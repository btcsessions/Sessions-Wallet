//! Group vault messaging: a Marmot (MLS over Nostr) runtime, from the Marmot
//! Development Kit's Android build (MarmotKit), in its own native library.
//!
//! It lives in a separate `.so` because MarmotKit links its own SQLCipher,
//! which can't share a binary with the SQLite the Lightning SDK links. The
//! wallet's Rust code talks to it through this plugin; the webview only gets
//! a "something changed" event (`marmot://changed`).
//!
//! The account key is the wallet's own Nostr key (from the seed). It is
//! handed to the runtime in memory on start and never written to disk by
//! the plugin.
//!
//! Desktop and iOS: not available yet; every call says so.

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime};

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("marmot")
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api.register_android_plugin("io.btcsessions.marmot", "MarmotPlugin")?;
                app.manage(Marmot::<R> { handle });
            }
            #[cfg(not(target_os = "android"))]
            app.manage(Marmot::<R> {
                _r: std::marker::PhantomData,
            });
            Ok(())
        })
        .build()
}

pub struct Marmot<R: Runtime> {
    #[cfg(target_os = "android")]
    handle: tauri::plugin::PluginHandle<R>,
    #[cfg(not(target_os = "android"))]
    _r: std::marker::PhantomData<fn() -> R>,
}

pub type Result<T> = std::result::Result<T, String>;

pub const UNAVAILABLE: &str = "Group vaults need the Android app for now";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The account (wallet public key, hex) the runtime is signed in as.
    pub account: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRow {
    pub group_id: String,
    pub name: String,
    /// An invite not yet accepted or declined.
    pub pending: bool,
    /// Who invited us (wallet public key, hex), for pending invites.
    #[serde(default)]
    pub welcomer: Option<String>,
    #[serde(default)]
    pub unread: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Members {
    pub members: Vec<String>,
    pub admins: Vec<String>,
}

/// A message as stored by the runtime (sender authenticated by the group).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub sender: String,
    pub kind: u16,
    pub content: String,
    pub created_at: u64,
}

#[derive(Deserialize)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct GroupId {
    group_id: String,
}

#[derive(Deserialize)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct List<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct Empty {}

#[cfg(target_os = "android")]
impl<R: Runtime> Marmot<R> {
    async fn call<T: serde::de::DeserializeOwned>(&self, method: &str, args: serde_json::Value) -> Result<T> {
        self.handle
            .run_mobile_plugin_async::<T>(method, args)
            .await
            .map_err(|e| match e {
                tauri::plugin::mobile::PluginInvokeError::InvokeRejected(r) => {
                    r.message.unwrap_or_else(|| "Group messaging failed".into())
                }
                other => other.to_string(),
            })
    }
}

#[cfg(target_os = "android")]
macro_rules! call {
    ($self:ident, $m:literal, $args:expr) => {
        $self.call($m, $args).await
    };
}

#[cfg(not(target_os = "android"))]
macro_rules! call {
    ($self:ident, $m:literal, $args:expr) => {{
        let _ = (&$self, $m, $args);
        Err(UNAVAILABLE.to_string())
    }};
}

impl<R: Runtime> Marmot<R> {
    pub fn available(&self) -> bool {
        cfg!(target_os = "android")
    }

    /// Open the runtime in `root` signed in as the wallet key, and connect.
    /// Idempotent: a second call with the same key returns the status.
    pub async fn start(&self, root: &str, relays: &[String], secret_hex: &str, pubkey_hex: &str) -> Result<Status> {
        call!(self, "start", serde_json::json!({
            "root": root, "relays": relays, "secretHex": secret_hex, "pubkeyHex": pubkey_hex,
        }))
    }

    /// Stop the runtime and forget the key (the wallet locked).
    pub async fn stop(&self) -> Result<()> {
        let _: Empty = call!(self, "stop", serde_json::json!({}))?;
        Ok(())
    }

    /// Make a new group with only us in it. Returns its id.
    pub async fn create_group(&self, name: &str) -> Result<String> {
        let g: GroupId = call!(self, "createGroup", serde_json::json!({ "name": name }))?;
        Ok(g.group_id)
    }

    pub async fn invite(&self, group: &str, members: &[String]) -> Result<()> {
        let _: Empty = call!(self, "invite", serde_json::json!({ "groupId": group, "members": members }))?;
        Ok(())
    }

    pub async fn remove(&self, group: &str, members: &[String]) -> Result<()> {
        let _: Empty = call!(self, "remove", serde_json::json!({ "groupId": group, "members": members }))?;
        Ok(())
    }

    pub async fn accept(&self, group: &str) -> Result<()> {
        let _: Empty = call!(self, "accept", serde_json::json!({ "groupId": group }))?;
        Ok(())
    }

    pub async fn decline(&self, group: &str) -> Result<()> {
        let _: Empty = call!(self, "decline", serde_json::json!({ "groupId": group }))?;
        Ok(())
    }

    pub async fn groups(&self) -> Result<Vec<GroupRow>> {
        let l: List<GroupRow> = call!(self, "groups", serde_json::json!({}))?;
        Ok(l.items)
    }

    pub async fn members(&self, group: &str) -> Result<Members> {
        call!(self, "members", serde_json::json!({ "groupId": group }))
    }

    /// A wallet message (custom kind) to the group.
    pub async fn send(&self, group: &str, kind: u16, content: &str) -> Result<()> {
        let _: Empty = call!(self, "send", serde_json::json!({ "groupId": group, "kind": kind, "content": content }))?;
        Ok(())
    }

    /// A chat message.
    pub async fn send_text(&self, group: &str, text: &str) -> Result<()> {
        let _: Empty = call!(self, "sendText", serde_json::json!({ "groupId": group, "text": text }))?;
        Ok(())
    }

    /// Stored messages, oldest first. `kinds` filters (empty = all).
    pub async fn messages(&self, group: &str, kinds: &[u16], limit: u32) -> Result<Vec<Message>> {
        let l: List<Message> = call!(self, "messages", serde_json::json!({ "groupId": group, "kinds": kinds, "limit": limit }))?;
        Ok(l.items)
    }
}
