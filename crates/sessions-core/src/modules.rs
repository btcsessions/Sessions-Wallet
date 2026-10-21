//! The user's home-screen layout: which modules are installed and in what order.
//!
//! The module catalogue (names, icons, descriptions) lives in the UI; the core
//! only persists the layout so it follows the wallet across devices via the
//! encrypted backup.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledModule {
    /// Stable catalogue id, e.g. `"balance"`, `"lightning"`, `"multisig"`.
    pub id: String,
    /// `"small" | "medium" | "large"` – the tile size on the home grid.
    pub size: String,
    /// Free-form per-module settings (selected account, chart range…).
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub version: u32,
    /// Shows advanced detail (fees, UTXOs, descriptors) everywhere.
    pub advanced: bool,
    pub modules: Vec<InstalledModule>,
}

impl Default for Layout {
    /// The "dead simple" starting point: balance, pay/receive, activity.
    fn default() -> Self {
        let m = |id: &str, size: &str| InstalledModule {
            id: id.into(),
            size: size.into(),
            config: serde_json::Value::Null,
        };
        Layout {
            version: 1,
            advanced: false,
            modules: vec![
                m("balance", "large"),
                m("pay-receive", "medium"),
                m("activity", "large"),
            ],
        }
    }
}
