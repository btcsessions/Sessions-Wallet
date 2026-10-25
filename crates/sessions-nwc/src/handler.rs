//! Request handling, independent of relays: an event in, maybe an event out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use bitcoin::secp256k1::{SecretKey, XOnlyPublicKey};
use lightning_invoice::Bolt11Invoice;
use serde_json::{json, Value};
use sessions_backup::keys::NostrKey;
use sessions_backup::nostr::Event;

use crate::budget::period_bounds;
use crate::connection::{Connection, Permission};
use crate::crypto::{secret_key_from_hex, xonly_from_hex};
use crate::protocol::*;
use crate::wallet::NwcWallet;
use crate::{nip04, nip44, now, Error};
use crate::{
    KIND_INFO, KIND_NOTIFICATION_NIP04, KIND_NOTIFICATION_NIP44, KIND_REQUEST, KIND_RESPONSE,
};

/// Event ids remembered to drop duplicates (relays resend on reconnect, and
/// the same request arrives from every relay of the connection).
const SEEN_LIMIT: usize = 10_000;
/// Most transactions returned by one `list_transactions` (relays limit
/// event sizes; NWC-05 suggests small pages).
pub const MAX_LIST_LIMIT: u64 = 100;
/// NWC-06: metadata longer than this is dropped.
const MAX_METADATA_LEN: usize = 4096;

/// Called with the connection after the service changed it (budget usage,
/// `last_used_at`, recent payments) so the app can persist it. Called
/// outside the service's locks; keep it quick (spawn if it does I/O).
pub trait ConnectionStore: Send + Sync + 'static {
    fn save(&self, connection: &Connection);
}

impl<F: Fn(&Connection) + Send + Sync + 'static> ConnectionStore for F {
    fn save(&self, connection: &Connection) {
        self(connection)
    }
}

/// Why a request got no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    WrongKind,
    BadSignature,
    /// No `p` tag, or not one of our connections.
    UnknownWallet,
    /// Signed by someone other than the connection's client.
    WrongAuthor,
    /// Past its `expiration` tag.
    Expired,
    Duplicate,
    /// The content couldn't be decrypted with the announced scheme.
    Undecryptable,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Ignored(Ignored),
    /// A signed kind-23195 event to publish on `relays`.
    Respond {
        relays: Vec<String>,
        event: Event,
    },
}

impl Outcome {
    pub fn response(&self) -> Option<&Event> {
        match self {
            Outcome::Respond { event, .. } => Some(event),
            Outcome::Ignored(_) => None,
        }
    }
}

struct Live {
    conn: Connection,
    sk: SecretKey,
    client_pk: XOnlyPublicKey,
    /// Millisats of payments in flight.
    reserved_msat: u64,
    in_flight: HashSet<String>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.sk.non_secure_erase();
    }
}

#[derive(Default)]
struct Seen {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    /// `false` if already there.
    fn insert(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        while self.order.len() > SEEN_LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

#[derive(Default)]
struct State {
    /// By wallet pubkey.
    live: HashMap<String, Live>,
    seen: Seen,
}

/// The wallet service core: connections, permissions, budgets, crypto.
/// Cheap to share (`Arc`); [`crate::service::start`] wraps it with relays.
pub struct WalletService<W: NwcWallet> {
    wallet: Arc<W>,
    store: Arc<dyn ConnectionStore>,
    state: Mutex<State>,
}

/// A decoded, authenticated request.
struct Request {
    wallet_pk: String,
    sk: SecretKey,
    client_pk: XOnlyPublicKey,
    relays: Vec<String>,
    encryption: Encryption,
    request_id: String,
}

impl<W: NwcWallet> WalletService<W> {
    pub fn new(wallet: Arc<W>, store: Arc<dyn ConnectionStore>) -> Self {
        WalletService {
            wallet,
            store,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the set of connections (after the user adds, edits or removes
    /// one). Settings come from `connections`; for ids already loaded, the
    /// service keeps its own usage counters (spent, last used, recent
    /// payments), which are newer than the app's copy. Use
    /// [`Self::reset_usage`] to clear a budget's spending on purpose.
    /// Returns the ids of connections that were skipped as invalid.
    pub fn set_connections(&self, connections: Vec<Connection>) -> Vec<String> {
        let mut rejected = Vec::new();
        let mut st = self.lock();
        let mut old: HashMap<String, Live> = st
            .live
            .drain()
            .map(|(_, l)| (l.conn.id.clone(), l))
            .collect();
        for mut conn in connections {
            let keys = conn.validate().and_then(|_| {
                Ok((
                    secret_key_from_hex(conn.wallet_secret.expose())?,
                    xonly_from_hex(&conn.client_pubkey)?,
                ))
            });
            let (sk, client_pk) = match keys {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(id = %conn.id, "nwc: skipping invalid connection: {e}");
                    rejected.push(conn.id);
                    continue;
                }
            };
            let (reserved_msat, in_flight) = match old.remove(&conn.id) {
                Some(mut prev) if prev.conn.wallet_pubkey == conn.wallet_pubkey => {
                    conn.usage = prev.conn.usage;
                    conn.last_used_at = prev.conn.last_used_at.max(conn.last_used_at);
                    conn.recent = std::mem::take(&mut prev.conn.recent);
                    (prev.reserved_msat, std::mem::take(&mut prev.in_flight))
                }
                _ => (0, HashSet::new()),
            };
            st.live.insert(
                conn.wallet_pubkey.clone(),
                Live {
                    conn,
                    sk,
                    client_pk,
                    reserved_msat,
                    in_flight,
                },
            );
        }
        rejected
    }

    /// The connections as the service has them now (with current usage).
    pub fn connections(&self) -> Vec<Connection> {
        let st = self.lock();
        let mut v: Vec<Connection> = st.live.values().map(|l| l.conn.clone()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        v
    }

    pub fn connection(&self, id: &str) -> Option<Connection> {
        self.lock()
            .live
            .values()
            .find(|l| l.conn.id == id)
            .map(|l| l.conn.clone())
    }

    /// Forget what a connection spent in the current period.
    pub fn reset_usage(&self, id: &str) -> Option<Connection> {
        let conn = {
            let mut st = self.lock();
            let l = st.live.values_mut().find(|l| l.conn.id == id)?;
            l.conn.usage.spent_msat = 0;
            l.conn.clone()
        };
        self.store.save(&conn);
        Some(conn)
    }

    /// Relay → wallet pubkeys to listen for there (active connections only).
    pub fn subscriptions(&self, now: u64) -> HashMap<String, Vec<String>> {
        let st = self.lock();
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for l in st.live.values().filter(|l| l.conn.is_active(now)) {
            for r in &l.conn.relays {
                m.entry(normalize_relay(r))
                    .or_default()
                    .push(l.conn.wallet_pubkey.clone());
            }
        }
        for v in m.values_mut() {
            v.sort();
            v.dedup();
        }
        m
    }

    /// The kind-13194 info event of every active connection.
    pub fn info_events(&self, now: u64) -> Vec<(Vec<String>, Event)> {
        let st = self.lock();
        st.live
            .values()
            .filter(|l| l.conn.is_active(now))
            .filter_map(|l| {
                let key = NostrKey::from_secret_bytes(&l.sk.secret_bytes()).ok()?;
                let methods: Vec<&str> = l.conn.methods().iter().map(|m| m.as_str()).collect();
                let mut tags = vec![vec!["encryption".to_string(), "nip44_v2 nip04".to_string()]];
                if l.conn.has(Permission::Notifications) {
                    tags.push(vec![
                        "notifications".into(),
                        "payment_received payment_sent".into(),
                    ]);
                }
                let ext = l.conn.extensions();
                if !ext.is_empty() {
                    tags.push(vec!["extensions".into(), ext.join(" ")]);
                }
                let ev = Event::sign(&key, now, KIND_INFO, tags, methods.join(" "));
                Some((l.conn.relays.clone(), ev))
            })
            .collect()
    }

    /// Notification events (kind 23197 NIP-44 and 23196 NIP-04, per NWC-02)
    /// for every active connection allowed to receive notifications.
    pub fn notification_events(
        &self,
        kind: NotificationType,
        tx: &Transaction,
        now: u64,
    ) -> Vec<(Vec<String>, Event)> {
        let body = json!({ "notification_type": kind.as_str(), "notification": tx }).to_string();
        let st = self.lock();
        let mut out = Vec::new();
        for l in st.live.values() {
            if !l.conn.is_active(now) || !l.conn.has(Permission::Notifications) {
                continue;
            }
            let Ok(key) = NostrKey::from_secret_bytes(&l.sk.secret_bytes()) else {
                continue;
            };
            let tags = vec![vec!["p".to_string(), l.conn.client_pubkey.clone()]];
            for enc in [Encryption::Nip44V2, Encryption::Nip04] {
                let Ok(content) = encrypt(enc, &l.sk, &l.client_pk, &body) else {
                    continue;
                };
                let k = match enc {
                    Encryption::Nip44V2 => KIND_NOTIFICATION_NIP44,
                    Encryption::Nip04 => KIND_NOTIFICATION_NIP04,
                };
                out.push((
                    l.conn.relays.clone(),
                    Event::sign(&key, now, k, tags.clone(), content),
                ));
            }
        }
        out
    }

    /// Handle one request event at the current time.
    pub async fn handle_event(&self, event: &Event) -> Outcome {
        self.handle_event_at(event, now()).await
    }

    /// Handle one request event as if it were `now` (unix seconds).
    pub async fn handle_event_at(&self, event: &Event, now: u64) -> Outcome {
        let (req, plaintext) = match self.authenticate(event, now) {
            Ok(r) => r,
            Err(Auth::Ignore(why)) => {
                tracing::debug!(id = %event.id, "nwc: ignoring request: {why:?}");
                return Outcome::Ignored(why);
            }
            Err(Auth::Reply(req, err)) => {
                return self.respond(&req, "", Err(err), now);
            }
        };
        let body: RequestBody = match serde_json::from_str(&plaintext) {
            Ok(b) => b,
            Err(_) => {
                return self.respond(&req, "", Err(NwcError::other("invalid request")), now);
            }
        };
        let result = self.dispatch(&req, &body, now).await;
        if let Err(e) = &result {
            tracing::info!(method = %body.method, code = ?e.code, "nwc: request failed");
        } else {
            tracing::info!(method = %body.method, "nwc: request handled");
        }
        self.respond(&req, &body.method, result, now)
    }

    fn authenticate(&self, event: &Event, now: u64) -> Result<(Request, String), Auth> {
        if event.kind != KIND_REQUEST {
            return Err(Auth::Ignore(Ignored::WrongKind));
        }
        let wallet_pk = event
            .tag("p")
            .and_then(|v| v.first())
            .ok_or(Auth::Ignore(Ignored::UnknownWallet))?
            .to_ascii_lowercase();
        let (sk, client_pk, relays) = {
            let st = self.lock();
            let l = st
                .live
                .get(&wallet_pk)
                .ok_or(Auth::Ignore(Ignored::UnknownWallet))?;
            if event.pubkey != l.conn.client_pubkey {
                return Err(Auth::Ignore(Ignored::WrongAuthor));
            }
            (l.sk, l.client_pk, l.conn.relays.clone())
        };
        if !event.verify() {
            return Err(Auth::Ignore(Ignored::BadSignature));
        }
        if let Some(exp) = event
            .tag("expiration")
            .and_then(|v| v.first())
            .and_then(|s| s.parse::<u64>().ok())
        {
            if now > exp {
                return Err(Auth::Ignore(Ignored::Expired));
            }
        }
        if !self.lock().seen.insert(&event.id) {
            return Err(Auth::Ignore(Ignored::Duplicate));
        }
        let mut req = Request {
            wallet_pk,
            sk,
            client_pk,
            relays,
            encryption: Encryption::Nip44V2,
            request_id: event.id.clone(),
        };
        let encryption = match event.tag("encryption").and_then(|v| v.first()) {
            // No tag: NIP-04 (legacy). Some clients send NIP-44 without the
            // tag; the payload format tells them apart.
            None if !event.content.contains("?iv=") => Encryption::Nip44V2,
            None => Encryption::Nip04,
            Some(v) => match v.split_whitespace().find_map(Encryption::parse) {
                Some(e) => e,
                None => {
                    return Err(Auth::Reply(
                        Box::new(req),
                        NwcError::new(
                            ErrorCode::UnsupportedEncryption,
                            format!("unsupported encryption: {v}"),
                        ),
                    ))
                }
            },
        };
        req.encryption = encryption;
        let plaintext = decrypt(encryption, &sk, &client_pk, &event.content)
            .map_err(|_| Auth::Ignore(Ignored::Undecryptable))?;
        Ok((req, plaintext))
    }

    fn respond(
        &self,
        req: &Request,
        method: &str,
        result: Result<Value, NwcError>,
        now: u64,
    ) -> Outcome {
        let body = match result {
            Ok(v) => ResponseBody {
                result_type: method.to_string(),
                error: None,
                result: Some(v),
            },
            Err(e) => ResponseBody {
                result_type: method.to_string(),
                error: Some(e),
                result: None,
            },
        };
        let json = serde_json::to_string(&body).expect("serialisable");
        let content = match encrypt(req.encryption, &req.sk, &req.client_pk, &json) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("nwc: couldn't encrypt response: {e}");
                return Outcome::Ignored(Ignored::Undecryptable);
            }
        };
        let key = NostrKey::from_secret_bytes(&req.sk.secret_bytes()).expect("valid key");
        let client_hex = req.client_pk.serialize();
        let tags = vec![
            vec![
                "p".to_string(),
                bitcoin::hex::DisplayHex::to_lower_hex_string(&client_hex[..]),
            ],
            vec!["e".to_string(), req.request_id.clone()],
        ];
        Outcome::Respond {
            relays: req.relays.clone(),
            event: Event::sign(&key, now, KIND_RESPONSE, tags, content),
        }
    }

    /// Run `f` on the live connection and persist the result.
    fn update<T>(&self, wallet_pk: &str, f: impl FnOnce(&mut Live) -> T) -> Option<T> {
        let (out, conn) = {
            let mut st = self.lock();
            let l = st.live.get_mut(wallet_pk)?;
            let out = f(l);
            (out, l.conn.clone())
        };
        self.store.save(&conn);
        Some(out)
    }

    async fn dispatch(
        &self,
        req: &Request,
        body: &RequestBody,
        now: u64,
    ) -> Result<Value, NwcError> {
        // Re-read the connection: settings may have changed since auth.
        let conn = self
            .update(&req.wallet_pk, |l| {
                l.conn.last_used_at = Some(now);
                l.conn.clone()
            })
            .ok_or_else(|| NwcError::new(ErrorCode::Unauthorized, "no wallet connected"))?;
        if !conn.enabled {
            return Err(NwcError::new(
                ErrorCode::Unauthorized,
                "this connection is disabled",
            ));
        }
        if !conn.is_active(now) {
            return Err(NwcError::new(
                ErrorCode::Unauthorized,
                "this connection has expired",
            ));
        }
        let Some(method) = Method::parse(&body.method) else {
            return Err(NwcError::not_implemented(format!(
                "{} is not supported",
                body.method
            )));
        };
        if !conn.allows(method) {
            return Err(NwcError::restricted(format!(
                "this connection isn't allowed to {}",
                method.as_str()
            )));
        }
        let params = if body.params.is_null() {
            json!({})
        } else {
            body.params.clone()
        };
        match method {
            Method::GetInfo => self.get_info(&conn).await,
            Method::GetBalance => {
                let b = self.wallet.balance_msat().await?;
                Ok(json!({ "balance": b }))
            }
            Method::GetBudget => Ok(self.get_budget(&req.wallet_pk, now)),
            Method::PayInvoice => self.pay_invoice(req, params, now).await,
            Method::MakeInvoice => self.make_invoice(req, params).await,
            Method::LookupInvoice => self.lookup_invoice(&conn, params).await,
            Method::ListTransactions => {
                let mut p: ListTransactionsRequest = parse_params(params)?;
                p.limit = Some(p.limit.unwrap_or(MAX_LIST_LIMIT).min(MAX_LIST_LIMIT));
                let list = self.wallet.list_transactions(p).await?;
                Ok(serde_json::to_value(list).expect("serialisable"))
            }
        }
    }

    async fn get_info(&self, conn: &Connection) -> Result<Value, NwcError> {
        let info = self.wallet.get_info().await?;
        let mut v = serde_json::to_value(info).expect("serialisable");
        let o = v.as_object_mut().expect("object");
        o.insert(
            "methods".into(),
            json!(conn
                .methods()
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()),
        );
        if conn.has(Permission::Notifications) {
            o.insert(
                "notifications".into(),
                json!(["payment_received", "payment_sent"]),
            );
        }
        let ext = conn.extensions();
        if !ext.is_empty() {
            o.insert("extensions".into(), json!(ext));
        }
        Ok(v)
    }

    fn get_budget(&self, wallet_pk: &str, now: u64) -> Value {
        let mut st = self.lock();
        let Some(l) = st.live.get_mut(wallet_pk) else {
            return json!({});
        };
        let b = l.conn.budget;
        let Some(limit) = b.limit_sat else {
            return json!({});
        };
        l.conn.usage.roll(b.period, l.conn.created_at, now);
        let (_, end) = period_bounds(b.period, l.conn.created_at, now);
        let mut v = json!({
            "used_budget": l.conn.usage.spent_msat.saturating_add(l.reserved_msat),
            "total_budget": limit.saturating_mul(1000),
            "renewal_period": b.period.renewal_period(),
        });
        if let Some(end) = end {
            v["renews_at"] = json!(end);
        }
        v
    }

    async fn pay_invoice(&self, req: &Request, params: Value, now: u64) -> Result<Value, NwcError> {
        let invoice = params
            .get("invoice")
            .and_then(Value::as_str)
            .ok_or_else(|| NwcError::other("missing invoice"))?
            .trim()
            .to_string();
        let requested = params.get("amount").and_then(Value::as_u64);
        let metadata = params.get("metadata").cloned().and_then(limit_metadata);
        let decoded = decode_invoice(&invoice)?;
        let amount_msat = match (decoded.amount_msat, requested) {
            (Some(a), Some(r)) if a != r => {
                return Err(NwcError::other("amount doesn't match the invoice"));
            }
            (Some(a), _) => a,
            (None, Some(r)) if r > 0 => r,
            (None, _) => return Err(NwcError::other("amount required for this invoice")),
        };
        let hash = decoded.payment_hash;

        // Paid before through this connection (a replayed request): don't
        // pay twice, hand back the preimage.
        let already = self
            .lock()
            .live
            .get(&req.wallet_pk)
            .is_some_and(|l| l.conn.knows(&hash, Some(TransactionType::Outgoing)));
        if already {
            let tx = self
                .wallet
                .lookup_invoice(LookupInvoiceRequest {
                    payment_hash: Some(hash.clone()),
                    invoice: Some(invoice.clone()),
                })
                .await?;
            return match tx.and_then(|t| t.preimage.map(|p| (p, t.fees_paid_msat))) {
                Some((preimage, fees)) => Ok(json!({ "preimage": preimage, "fees_paid": fees })),
                None => Err(NwcError::other("this invoice was already paid")),
            };
        }

        // Reserve the amount against the budget.
        {
            let mut st = self.lock();
            let l = st
                .live
                .get_mut(&req.wallet_pk)
                .ok_or_else(|| NwcError::new(ErrorCode::Unauthorized, "no wallet connected"))?;
            let b = l.conn.budget;
            if let Some(max) = b.max_per_payment_sat {
                if amount_msat > max.saturating_mul(1000) {
                    return Err(NwcError::quota_exceeded(format!(
                        "payments over {max} sats aren't allowed for this connection"
                    )));
                }
            }
            l.conn.usage.roll(b.period, l.conn.created_at, now);
            if let Some(limit) = b.limit_sat {
                let used = l.conn.usage.spent_msat.saturating_add(l.reserved_msat);
                if used.saturating_add(amount_msat) > limit.saturating_mul(1000) {
                    return Err(NwcError::quota_exceeded(
                        "this payment would go over the connection's budget",
                    ));
                }
            }
            if !l.in_flight.insert(hash.clone()) {
                return Err(NwcError::other("this invoice is already being paid"));
            }
            l.reserved_msat = l.reserved_msat.saturating_add(amount_msat);
        }

        let result = self
            .wallet
            .pay_invoice(PayInvoiceRequest {
                invoice,
                amount_msat: decoded.amount_msat.is_none().then_some(amount_msat),
                total_amount_msat: amount_msat,
                payment_hash: hash.clone(),
                metadata,
            })
            .await;

        // Fees paid when it went through; zero when it may still go through
        // (counted in full and never paid again); nothing when it failed.
        let ok = match &result {
            Ok(r) => Some(r.fees_paid_msat),
            Err(e) if e.in_flight => Some(0),
            Err(_) => None,
        };
        // Release the reservation; count it if it went (or may go) through.
        // If the connection was removed meanwhile there's nothing to update.
        self.update(&req.wallet_pk, |l| {
            l.reserved_msat = l.reserved_msat.saturating_sub(amount_msat);
            l.in_flight.remove(&hash);
            if let Some(fees) = ok {
                l.conn
                    .usage
                    .roll(l.conn.budget.period, l.conn.created_at, now);
                l.conn.usage.spent_msat = l
                    .conn
                    .usage
                    .spent_msat
                    .saturating_add(amount_msat)
                    .saturating_add(fees);
                l.conn.remember(&hash, TransactionType::Outgoing);
            }
        });
        let r = result?;
        Ok(json!({ "preimage": r.preimage, "fees_paid": r.fees_paid_msat }))
    }

    async fn make_invoice(&self, req: &Request, params: Value) -> Result<Value, NwcError> {
        let mut p: MakeInvoiceRequest = parse_params(params)?;
        p.metadata = p.metadata.and_then(limit_metadata);
        let tx = self.wallet.make_invoice(p).await?;
        let hash = tx.payment_hash.clone();
        self.update(&req.wallet_pk, |l| {
            l.conn.remember(&hash, TransactionType::Incoming)
        });
        Ok(serde_json::to_value(tx).expect("serialisable"))
    }

    async fn lookup_invoice(&self, conn: &Connection, params: Value) -> Result<Value, NwcError> {
        let mut p: LookupInvoiceRequest = parse_params(params)?;
        let from_invoice = match &p.invoice {
            Some(inv) => Some(decode_invoice(inv.trim())?.payment_hash),
            None => None,
        };
        let hash = match (p.payment_hash.as_deref(), from_invoice) {
            (Some(h), Some(i)) if !h.eq_ignore_ascii_case(&i) => {
                return Err(NwcError::other("payment_hash doesn't match the invoice"));
            }
            (Some(h), _) => h.to_ascii_lowercase(),
            (None, Some(i)) => i,
            (None, None) => return Err(NwcError::other("payment_hash or invoice required")),
        };
        // Without History, only what this connection made or paid, and
        // without saying whether anything else exists.
        if !conn.has(Permission::History) && !conn.knows(&hash, None) {
            return Err(NwcError::not_found("invoice not found"));
        }
        p.payment_hash = Some(hash);
        match self.wallet.lookup_invoice(p).await? {
            Some(tx) => Ok(serde_json::to_value(tx).expect("serialisable")),
            None => Err(NwcError::not_found("invoice not found")),
        }
    }
}

enum Auth {
    Ignore(Ignored),
    Reply(Box<Request>, NwcError),
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, NwcError> {
    serde_json::from_value(params).map_err(|e| NwcError::other(format!("invalid params: {e}")))
}

fn limit_metadata(v: Value) -> Option<Value> {
    (v.to_string().len() <= MAX_METADATA_LEN).then_some(v)
}

pub(crate) struct DecodedInvoice {
    pub amount_msat: Option<u64>,
    pub payment_hash: String,
}

/// Amount and payment hash of a BOLT11 invoice (checksum and signature checked).
pub(crate) fn decode_invoice(invoice: &str) -> Result<DecodedInvoice, NwcError> {
    let s = invoice.strip_prefix("lightning:").unwrap_or(invoice);
    let inv = Bolt11Invoice::from_str(s).map_err(|_| NwcError::other("invalid invoice"))?;
    Ok(DecodedInvoice {
        amount_msat: inv.amount_milli_satoshis(),
        payment_hash: inv.payment_hash().to_string(),
    })
}

pub(crate) fn encrypt(
    enc: Encryption,
    sk: &SecretKey,
    pk: &XOnlyPublicKey,
    plaintext: &str,
) -> Result<String, Error> {
    match enc {
        Encryption::Nip44V2 => nip44::encrypt(&nip44::ConversationKey::new(sk, pk), plaintext),
        Encryption::Nip04 => Ok(nip04::encrypt(sk, pk, plaintext)),
    }
}

pub(crate) fn decrypt(
    enc: Encryption,
    sk: &SecretKey,
    pk: &XOnlyPublicKey,
    payload: &str,
) -> Result<String, Error> {
    match enc {
        Encryption::Nip44V2 => nip44::decrypt(&nip44::ConversationKey::new(sk, pk), payload),
        Encryption::Nip04 => nip04::decrypt(sk, pk, payload),
    }
}

/// Relay URLs compare without a trailing slash.
pub(crate) fn normalize_relay(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}
