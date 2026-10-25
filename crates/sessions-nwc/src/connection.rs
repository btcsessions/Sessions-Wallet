//! A connection: one client app's access to the wallet, with its own wallet
//! service key, permissions and budget. Persisted by the app as JSON.

use std::collections::BTreeSet;

use rand::RngCore;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sessions_backup::keys::NostrKey;
use zeroize::Zeroizing;

use crate::budget::{Budget, BudgetUsage};
use crate::crypto::{secret_key_from_hex, xonly_from_hex};
use crate::protocol::{Method, TransactionType};
use crate::Error;

/// URI scheme of connection strings.
pub const URI_SCHEME: &str = "nostr+walletconnect://";

/// How many recent payment hashes a connection remembers (for
/// `lookup_invoice` visibility and double-payment protection).
pub const RECENT_LIMIT: usize = 200;

/// What a connection may do. `get_info` is always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// `pay_invoice` (within the budget) and `get_budget`.
    Pay,
    /// `make_invoice`.
    Receive,
    /// `get_balance`.
    Balance,
    /// `list_transactions`, and `lookup_invoice` on any invoice or payment
    /// (without it, only on those this connection made).
    History,
    /// `payment_received` / `payment_sent` notifications.
    Notifications,
}

impl Permission {
    pub const ALL: [Permission; 5] = [
        Permission::Pay,
        Permission::Receive,
        Permission::Balance,
        Permission::History,
        Permission::Notifications,
    ];
}

/// A hex secret key that doesn't show up in `Debug` output and is wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretHex(Zeroizing<String>);

impl SecretHex {
    pub fn new(hex: impl Into<String>) -> Self {
        SecretHex(Zeroizing::new(hex.into()))
    }
    /// The hex string. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretHex(…)")
    }
}

impl Serialize for SecretHex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretHex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SecretHex(Zeroizing::new(String::deserialize(d)?)))
    }
}

/// A payment hash this connection created (incoming) or paid (outgoing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPayment {
    pub payment_hash: String,
    #[serde(rename = "type")]
    pub kind: TransactionType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    /// Random id (hex), stable for the connection's life.
    pub id: String,
    /// What the user called it ("Primal", "Damus on the iPad"…).
    pub name: String,
    /// Unix seconds. Also the anchor of the budget periods.
    pub created_at: u64,
    #[serde(default)]
    pub last_used_at: Option<u64>,
    /// The wallet service's key for this connection only. Secret: store it
    /// with the rest of the wallet's secrets.
    pub wallet_secret: SecretHex,
    /// Its public key (the URI's host part, the `p` of requests).
    pub wallet_pubkey: String,
    /// Public key of the secret handed to the client in the URI. The secret
    /// itself is not kept (NIP-47: the service SHOULD NOT store it).
    pub client_pubkey: String,
    pub relays: Vec<String>,
    pub permissions: BTreeSet<Permission>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub usage: BudgetUsage,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Unix seconds after which the connection stops working.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Lightning address handed to the client in the URI (`lud16`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lud16: Option<String>,
    /// Most recent last.
    #[serde(default)]
    pub recent: Vec<RecentPayment>,
}

fn yes() -> bool {
    true
}

/// A connection just created, with the one-time client secret and URI. Show
/// the URI (QR / copy) and then drop it: only `connection` is persisted.
pub struct NewConnection {
    pub connection: Connection,
    pub client_secret: Zeroizing<String>,
    pub uri: Zeroizing<String>,
}

impl std::fmt::Debug for NewConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewConnection")
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}

/// Options for [`Connection::create`].
#[derive(Debug, Clone, Default)]
pub struct ConnectionOptions {
    pub name: String,
    pub relays: Vec<String>,
    pub permissions: BTreeSet<Permission>,
    pub budget: Budget,
    pub expires_at: Option<u64>,
    pub lud16: Option<String>,
}

impl Connection {
    /// A new connection with fresh wallet and client keys.
    pub fn create(opts: ConnectionOptions, now: u64) -> Result<NewConnection, Error> {
        if opts.relays.is_empty() {
            return Err(Error::Invalid(
                "a connection needs at least one relay".into(),
            ));
        }
        for r in &opts.relays {
            if !r.starts_with("wss://") && !r.starts_with("ws://") {
                return Err(Error::Invalid(format!("not a relay address: {r}")));
            }
        }
        let wallet = NostrKey::generate();
        let client = NostrKey::generate();
        let mut id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id);
        let connection = Connection {
            id: bitcoin::hex::DisplayHex::to_lower_hex_string(&id[..]),
            name: opts.name,
            created_at: now,
            last_used_at: None,
            wallet_secret: SecretHex(wallet.secret_hex()),
            wallet_pubkey: wallet.public_hex(),
            client_pubkey: client.public_hex(),
            relays: opts.relays,
            permissions: opts.permissions,
            budget: opts.budget,
            usage: BudgetUsage {
                spent_msat: 0,
                period_start: now,
            },
            enabled: true,
            expires_at: opts.expires_at,
            lud16: opts.lud16,
            recent: Vec::new(),
        };
        let client_secret = client.secret_hex();
        let uri = connection.uri(&client_secret);
        Ok(NewConnection {
            connection,
            client_secret,
            uri,
        })
    }

    /// `nostr+walletconnect://<wallet pubkey>?relay=…&secret=…[&lud16=…]`.
    pub fn uri(&self, client_secret_hex: &str) -> Zeroizing<String> {
        ConnectionUri {
            wallet_pubkey: self.wallet_pubkey.clone(),
            relays: self.relays.clone(),
            secret: SecretHex::new(client_secret_hex),
            lud16: self.lud16.clone(),
        }
        .to_uri()
    }

    /// The keys parse and match.
    pub fn validate(&self) -> Result<(), Error> {
        let sk = secret_key_from_hex(self.wallet_secret.expose())?;
        if crate::crypto::public_hex(&sk) != self.wallet_pubkey {
            return Err(Error::Key(
                "wallet secret doesn't match wallet pubkey".into(),
            ));
        }
        xonly_from_hex(&self.client_pubkey)?;
        if self.relays.is_empty() {
            return Err(Error::Invalid("no relays".into()));
        }
        Ok(())
    }

    pub fn has(&self, p: Permission) -> bool {
        self.permissions.contains(&p)
    }

    /// Whether this connection may call `m` at all.
    pub fn allows(&self, m: Method) -> bool {
        match m {
            Method::GetInfo => true,
            Method::PayInvoice | Method::GetBudget => self.has(Permission::Pay),
            Method::MakeInvoice => self.has(Permission::Receive),
            Method::GetBalance => self.has(Permission::Balance),
            Method::ListTransactions => self.has(Permission::History),
            Method::LookupInvoice => {
                self.has(Permission::History)
                    || self.has(Permission::Pay)
                    || self.has(Permission::Receive)
            }
        }
    }

    /// The methods advertised to this connection.
    pub fn methods(&self) -> Vec<Method> {
        Method::ALL
            .into_iter()
            .filter(|m| self.allows(*m))
            .collect()
    }

    /// Optional NWC extension specs this connection can use.
    pub fn extensions(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.has(Permission::Notifications) {
            v.push("02");
        }
        if self.has(Permission::History) {
            v.push("05");
        }
        if self.has(Permission::Pay) {
            v.push("13");
        }
        v
    }

    /// Enabled and not expired.
    pub fn is_active(&self, now: u64) -> bool {
        self.enabled && self.expires_at.is_none_or(|t| now < t)
    }

    pub(crate) fn remember(&mut self, payment_hash: &str, kind: TransactionType) {
        if self
            .recent
            .iter()
            .any(|r| r.payment_hash == payment_hash && r.kind == kind)
        {
            return;
        }
        self.recent.push(RecentPayment {
            payment_hash: payment_hash.to_string(),
            kind,
        });
        if self.recent.len() > RECENT_LIMIT {
            let excess = self.recent.len() - RECENT_LIMIT;
            self.recent.drain(..excess);
        }
    }

    pub(crate) fn knows(&self, payment_hash: &str, kind: Option<TransactionType>) -> bool {
        self.recent
            .iter()
            .any(|r| r.payment_hash == payment_hash && kind.is_none_or(|k| k == r.kind))
    }
}

/// A parsed connection string (what a client does with the URI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionUri {
    pub wallet_pubkey: String,
    pub relays: Vec<String>,
    pub secret: SecretHex,
    pub lud16: Option<String>,
}

impl ConnectionUri {
    pub fn to_uri(&self) -> Zeroizing<String> {
        let mut s = format!("{URI_SCHEME}{}", self.wallet_pubkey);
        let mut sep = '?';
        for r in &self.relays {
            s.push(sep);
            s.push_str("relay=");
            s.push_str(&percent_encode(r));
            sep = '&';
        }
        s.push(sep);
        s.push_str("secret=");
        s.push_str(self.secret.expose());
        if let Some(l) = &self.lud16 {
            s.push_str("&lud16=");
            s.push_str(&percent_encode(l));
        }
        Zeroizing::new(s)
    }

    pub fn parse(uri: &str) -> Result<Self, Error> {
        let bad = |m: &str| Error::Invalid(format!("not a wallet connect URI: {m}"));
        let rest = uri
            .strip_prefix(URI_SCHEME)
            .or_else(|| uri.strip_prefix("nostrwalletconnect://"))
            .ok_or_else(|| bad("scheme"))?;
        let (pk, query) = rest.split_once('?').ok_or_else(|| bad("no parameters"))?;
        let pk = pk.trim_end_matches('/').to_ascii_lowercase();
        xonly_from_hex(&pk)?;
        let mut relays = Vec::new();
        let mut secret = None;
        let mut lud16 = None;
        for kv in query.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let v = percent_decode(v).ok_or_else(|| bad("bad escape"))?;
            match k {
                "relay" => relays.push(v),
                "secret" => secret = Some(SecretHex::new(v.to_ascii_lowercase())),
                "lud16" => lud16 = Some(v),
                _ => {}
            }
        }
        let secret = secret.ok_or_else(|| bad("no secret"))?;
        secret_key_from_hex(secret.expose())?;
        if relays.is_empty() {
            return Err(bad("no relay"));
        }
        Ok(ConnectionUri {
            wallet_pubkey: pk,
            relays,
            secret,
            lud16,
        })
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ConnectionOptions {
        ConnectionOptions {
            name: "Primal".into(),
            relays: vec!["wss://relay.primal.net".into()],
            permissions: Permission::ALL.into_iter().collect(),
            budget: Budget::default(),
            expires_at: None,
            lud16: Some("ben@example.com".into()),
        }
    }

    #[test]
    fn spec_example_uri_parses() {
        let u = ConnectionUri::parse("nostr+walletconnect://b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4?relay=wss%3A%2F%2Frelay.damus.io&secret=71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c").unwrap();
        assert_eq!(
            u.wallet_pubkey,
            "b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4"
        );
        assert_eq!(u.relays, vec!["wss://relay.damus.io"]);
        assert_eq!(
            u.secret.expose(),
            "71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c"
        );
        // And serialises back to exactly the spec's string.
        assert_eq!(u.to_uri().as_str(), "nostr+walletconnect://b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4?relay=wss%3A%2F%2Frelay.damus.io&secret=71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c");
    }

    #[test]
    fn create_round_trips_through_uri() {
        let mut o = opts();
        o.relays.push("wss://relay.damus.io/".into());
        let n = Connection::create(o, 1_700_000_000).unwrap();
        n.connection.validate().unwrap();
        let u = ConnectionUri::parse(&n.uri).unwrap();
        assert_eq!(u.wallet_pubkey, n.connection.wallet_pubkey);
        assert_eq!(
            u.relays,
            vec!["wss://relay.primal.net", "wss://relay.damus.io/"]
        );
        assert_eq!(u.lud16.as_deref(), Some("ben@example.com"));
        assert!(n.uri.contains("&lud16=ben%40example.com"));
        assert!(n.uri.starts_with(&format!(
            "nostr+walletconnect://{}?relay=wss%3A%2F%2Frelay.primal.net&relay=",
            n.connection.wallet_pubkey
        )));
        // The client secret matches the stored client pubkey.
        let sk = secret_key_from_hex(u.secret.expose()).unwrap();
        assert_eq!(crate::crypto::public_hex(&sk), n.connection.client_pubkey);
        // Wallet and client keys differ, and differ per connection.
        assert_ne!(n.connection.wallet_pubkey, n.connection.client_pubkey);
        let m = Connection::create(opts(), 1).unwrap();
        assert_ne!(m.connection.wallet_pubkey, n.connection.wallet_pubkey);
        assert_ne!(m.connection.id, n.connection.id);
    }

    #[test]
    fn persists_as_json_without_leaking_in_debug() {
        let n = Connection::create(opts(), 5).unwrap();
        let json = serde_json::to_string(&n.connection).unwrap();
        let back: Connection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n.connection);
        assert!(json.contains(n.connection.wallet_secret.expose()));
        assert!(json.contains(
            "\"permissions\":[\"pay\",\"receive\",\"balance\",\"history\",\"notifications\"]"
        ));
        let dbg = format!("{n:?}");
        assert!(!dbg.contains(n.connection.wallet_secret.expose()));
        assert!(!dbg.contains(n.client_secret.as_str()));
        // The client secret is not part of the stored connection.
        assert!(!json.contains(n.client_secret.as_str()));
        // Older / minimal JSON fills defaults.
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in [
            "budget",
            "usage",
            "enabled",
            "recent",
            "last_used_at",
            "expires_at",
        ] {
            o.remove(k);
        }
        let min: Connection = serde_json::from_value(v).unwrap();
        assert!(min.enabled);
        assert!(min.budget.is_unlimited());
    }

    #[test]
    fn permissions_map_to_methods() {
        let mut c = Connection::create(opts(), 5).unwrap().connection;
        assert_eq!(c.methods().len(), Method::ALL.len());
        c.permissions = [Permission::Receive].into_iter().collect();
        assert_eq!(
            c.methods(),
            vec![Method::GetInfo, Method::MakeInvoice, Method::LookupInvoice]
        );
        assert!(c.extensions().is_empty());
        c.permissions.clear();
        assert_eq!(c.methods(), vec![Method::GetInfo]);
    }

    #[test]
    fn rejects_bad_input() {
        let mut o = opts();
        o.relays = vec!["https://relay.primal.net".into()];
        assert!(Connection::create(o, 1).is_err());
        let mut o = opts();
        o.relays.clear();
        assert!(Connection::create(o, 1).is_err());
        assert!(ConnectionUri::parse("nostr+walletconnect://abc?relay=x&secret=00").is_err());
        let mut c = Connection::create(opts(), 5).unwrap().connection;
        c.wallet_pubkey = c.client_pubkey.clone();
        assert!(c.validate().is_err());
    }

    #[test]
    fn recent_is_bounded() {
        let mut c = Connection::create(opts(), 5).unwrap().connection;
        for i in 0..(RECENT_LIMIT + 10) {
            c.remember(&format!("{i:064x}"), TransactionType::Outgoing);
        }
        c.remember(
            &format!("{:064x}", RECENT_LIMIT + 9),
            TransactionType::Outgoing,
        );
        assert_eq!(c.recent.len(), RECENT_LIMIT);
        assert!(!c.knows(&format!("{:064x}", 0), None));
        assert!(c.knows(&format!("{:064x}", 10), Some(TransactionType::Outgoing)));
        assert!(!c.knows(&format!("{:064x}", 10), Some(TransactionType::Incoming)));
    }
}
