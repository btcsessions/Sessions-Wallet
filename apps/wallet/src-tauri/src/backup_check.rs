//! "Test my backup": proves a backup would bring *this* wallet back, without
//! changing anything (no new wallet, nothing applied or uploaded). The only
//! write is `backup.lastTestedAt` in the config when the test passes, which
//! is local and not part of the online backup.
//!
//! * `passkey`: sign in with a passkey and work out the seed it would
//!   restore exactly as `restore_with_passkey` does (its envelope's phrase,
//!   else the passkey's own seed), then compare it with this wallet's.
//! * `phrase`: compare a typed recovery phrase with this wallet's.
//! * `state`: no prompt; only the state check below.
//!
//! Then (when the seed matches) the state backup is fetched and decrypted
//! with this seed's keys, as `restore_state` would, but not applied.
//!
//! Seeds are compared by the Nostr key derived from them for the state
//! backup (public, and equal only for the same seed): no seed or phrase is
//! returned or logged.

use serde::Serialize;
use serde_json::Value;
use sessions_backup::keys::{PasskeyKeys, StateKeys};
use sessions_backup::nostr::Event;
use sessions_backup::relay::PerRelay;
use sessions_backup::state::{self as snapshot, Snapshot};
use sessions_backup::KIND_APP_DATA;
use sessions_core::keystore::Seed;
use sessions_core::{Error, Network};
use tauri::{AppHandle, State};
use tauri_plugin_passkey::PasskeyError;
use zeroize::Zeroizing;

use crate::backup::{counts, now_secs, MIN_ANSWERS};
use crate::config::SeedOrigin;
use crate::passkey::{passkey_seed, sign_in, PasskeySeed};
use crate::state::AppState;

/// `test_backup`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupTest {
    /// The backup would restore this wallet, with its latest state.
    pub ok: bool,
    /// The passkey or phrase restores this wallet's seed; `None` for `state`
    /// (and when the relays couldn't be reached to tell).
    pub seed_matches: Option<bool>,
    /// A state backup for this wallet was found and decrypted.
    pub state_found: bool,
    pub state_version: Option<u64>,
    /// Version this device last uploaded (or restored).
    pub local_version: Option<u64>,
    /// The relays have this device's latest upload (or newer).
    pub up_to_date: bool,
    /// Unix seconds when the backup found was made.
    pub backed_up_at: Option<u64>,
    /// Backup relays that each hold a full copy of the newest backup.
    pub relays_with_latest: u32,
    pub relays_answered: u32,
    pub relays_total: u32,
    pub accounts: u32,
    pub hardware_wallets: u32,
    pub labels: u32,
    /// One plain-English summary.
    pub message: String,
}

/// Why `test_backup` couldn't run, for the UI to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCode {
    /// The user backed out of the passkey prompt: say nothing.
    Cancelled,
    /// No passkey for this app on the phone or in the account.
    NoPasskey,
    /// No passkeys here, no PRF, or the domain isn't set up for the app.
    Unsupported,
    /// `phrase`: not a valid recovery phrase (or none given).
    InvalidPhrase,
    Locked,
    Failed,
}

/// `test_backup`'s error: `{ "code": "cancelled", "message": "…" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestFailure {
    pub code: TestCode,
    pub message: String,
}

fn failure(code: TestCode, message: impl Into<String>) -> TestFailure {
    TestFailure {
        code,
        message: message.into(),
    }
}

impl From<Error> for TestFailure {
    fn from(e: Error) -> Self {
        let code = match e {
            Error::Locked => TestCode::Locked,
            _ => TestCode::Failed,
        };
        failure(code, e.to_string())
    }
}

fn from_passkey(e: PasskeyError) -> TestFailure {
    let code = match &e {
        PasskeyError::Cancelled => TestCode::Cancelled,
        PasskeyError::NoCredential => TestCode::NoPasskey,
        PasskeyError::NotSupported | PasskeyError::PrfUnsupported | PasskeyError::NotTrusted(_) => {
            TestCode::Unsupported
        }
        PasskeyError::Other(_) => TestCode::Failed,
    };
    failure(code, e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Passkey,
    Phrase,
    State,
}

impl Method {
    fn parse(s: &str) -> Result<Self, TestFailure> {
        match s {
            "passkey" => Ok(Method::Passkey),
            "phrase" => Ok(Method::Phrase),
            "state" => Ok(Method::State),
            _ => Err(failure(
                TestCode::Failed,
                format!("unknown backup test: {s}"),
            )),
        }
    }
}

/// How the seed check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeedCheck {
    /// It restores this wallet's seed (`envelope`: via the phrase sealed
    /// under the passkey, else the passkey's own seed or the typed phrase).
    Matches { envelope: bool },
    /// Another wallet's passkey or phrase.
    Different,
    /// This phrase wallet's passkey, but the phrase backup isn't on the
    /// relays: the passkey alone would restore an empty passkey wallet.
    EnvelopeMissing,
    /// Too few relays answered to tell.
    Unreachable { answered: u32, total: u32 },
}

/// What the passkey lookup found, without the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Envelope,
    Derived,
    Unreachable { answered: u32, total: u32 },
}

/// Judge a passkey: `matches` (the seed it restores is this wallet's),
/// `origin` of this wallet, and whether the passkey picked is the one made
/// on this wallet (`None`: none recorded).
fn passkey_outcome(
    found: Found,
    matches: bool,
    origin: SeedOrigin,
    same_credential: Option<bool>,
) -> SeedCheck {
    match found {
        Found::Unreachable { answered, total } => SeedCheck::Unreachable { answered, total },
        _ if matches => SeedCheck::Matches {
            envelope: found == Found::Envelope,
        },
        Found::Envelope => SeedCheck::Different,
        // A phrase wallet whose own passkey (or a passkey we can't rule
        // out) has no envelope on the relays.
        Found::Derived if origin == SeedOrigin::Phrase && same_credential != Some(false) => {
            SeedCheck::EnvelopeMissing
        }
        Found::Derived => SeedCheck::Different,
    }
}

/// The newest state backup, as a restore would find it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Latest {
    version: u64,
    created_at: u64,
    accounts: u32,
    hardware: u32,
    labels: u32,
    labels_omitted: bool,
}

/// What the relays hold of this seed's state backup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StateCheck {
    /// Enough relays answered to trust a "nothing there".
    reachable: bool,
    latest: Option<Latest>,
    /// Over the relays the backup is written to.
    with_latest: u32,
    answered: u32,
    total: u32,
}

fn hardware_count(snap: &Snapshot) -> u32 {
    snap.data["config"]["accounts"].as_array().map_or(0, |a| {
        a.iter()
            .filter(|a| a["kind"].as_str() == Some("hardware"))
            .count() as u32
    })
}

/// Decode what the relays sent, as `restore_state` would (every relay's
/// events together), and count the write relays that each hold the newest
/// copy on their own.
fn check_state(keys: &StateKeys, answers: &PerRelay<Vec<Event>>, write: &[String]) -> StateCheck {
    let reached = answers.iter().filter(|(_, r)| r.is_ok()).count();
    let all: Vec<Event> = answers
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .flatten()
        .cloned()
        .collect();
    let latest = snapshot::decode(keys, &all).map(|snap| {
        let (accounts, labels) = counts(&snap);
        Latest {
            version: snap.version,
            created_at: snap.created_at,
            accounts,
            hardware: hardware_count(&snap),
            labels,
            labels_omitted: snap.data["labelsOmitted"] == Value::Bool(true),
        }
    });
    // The relays the backup goes to; all of them if there are none.
    let counted: Vec<&(String, _)> = answers
        .iter()
        .filter(|(u, _)| write.is_empty() || write.contains(u))
        .collect();
    let with_latest = latest.as_ref().map_or(0, |l| {
        counted
            .iter()
            .filter(|(_, r)| {
                r.as_ref().is_ok_and(|events| {
                    snapshot::decode(keys, events)
                        .is_some_and(|s| (s.version, s.created_at) == (l.version, l.created_at))
                })
            })
            .count() as u32
    });
    StateCheck {
        reachable: reached >= MIN_ANSWERS.min(answers.len()),
        latest,
        with_latest,
        answered: counted.iter().filter(|(_, r)| r.is_ok()).count() as u32,
        total: counted.len() as u32,
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "5 accounts, 2 hardware wallets and 143 labels".
fn contents(l: &Latest) -> String {
    let mut parts = vec![plural(l.accounts, "account", "accounts")];
    if l.hardware > 0 {
        parts.push(plural(l.hardware, "hardware wallet", "hardware wallets"));
    }
    if l.labels > 0 {
        parts.push(plural(l.labels, "label", "labels"));
    }
    match parts.len() {
        1 => parts.remove(0),
        n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
    }
}

const UNREACHABLE: &str = "Couldn't reach the backup servers. Check your connection and try again.";

/// The answer, from the seed check (`None` for `state`) and the state check
/// (`None` when the seed check already failed).
fn summarize(
    method: Method,
    seed: Option<SeedCheck>,
    state: Option<&StateCheck>,
    local_version: Option<u64>,
) -> BackupTest {
    let mut t = BackupTest {
        local_version,
        ..Default::default()
    };
    match seed {
        Some(SeedCheck::Matches { .. }) => t.seed_matches = Some(true),
        Some(SeedCheck::Unreachable { answered, total }) => {
            t.relays_answered = answered;
            t.relays_total = total;
            t.message = UNREACHABLE.into();
            return t;
        }
        Some(other) => {
            t.seed_matches = Some(false);
            t.message = match (method, other) {
                (Method::Phrase, _) => "That recovery phrase is for a different wallet. Check each word against what you wrote down.",
                (_, SeedCheck::EnvelopeMissing) => "This passkey would restore a different wallet: your recovery phrase backup isn't on it. Turn on passkey backup again.",
                _ => "That passkey belongs to a different wallet.",
            }
            .into();
            return t;
        }
        None => {}
    }
    let Some(s) = state else {
        t.message = UNREACHABLE.into();
        return t;
    };
    t.relays_answered = s.answered;
    t.relays_total = s.total;
    t.relays_with_latest = s.with_latest;
    let lead = match method {
        Method::Passkey => "Your passkey restores this wallet",
        Method::Phrase => "Your recovery phrase restores this wallet",
        Method::State => "Your online backup is up to date",
    };
    let Some(l) = &s.latest else {
        t.message = if !s.reachable {
            UNREACHABLE.into()
        } else if method == Method::State {
            "There's no backup of this wallet's accounts and labels on the backup servers yet. Tap Back up now.".into()
        } else {
            format!("{lead}'s keys, but there's no backup of its accounts and labels on the backup servers yet. Tap Back up now.")
        };
        return t;
    };
    t.state_found = true;
    t.state_version = Some(l.version);
    t.backed_up_at = Some(l.created_at);
    t.accounts = l.accounts;
    t.hardware_wallets = l.hardware;
    t.labels = l.labels;
    t.up_to_date = local_version.is_none_or(|v| l.version >= v);
    if !t.up_to_date {
        t.message = match method {
            Method::State => "The backup servers only have an older copy of this wallet's settings. Tap Back up now.".into(),
            _ => format!("{lead}, but the backup servers only have an older copy of its settings. Tap Back up now."),
        };
        return t;
    }
    t.ok = true;
    let servers = if s.with_latest == s.total && s.total > 1 {
        format!("All {} backup servers have the latest copy.", s.total)
    } else {
        format!(
            "{} of {} backup servers {} the latest copy.",
            s.with_latest,
            s.total,
            if s.with_latest == 1 { "has" } else { "have" }
        )
    };
    let omitted = if l.labels_omitted {
        " Labels aren't included: there are too many for the online backup."
    } else {
        ""
    };
    t.message = format!("{lead}, with {}. {servers}{omitted}", contents(l));
    t
}

/// The key a seed's state backup is found under: equal only for the same seed.
fn seed_id(seed: &Seed, network: Network) -> Result<String, Error> {
    Ok(StateKeys::from_master(&seed.master_xpriv(network)?)?
        .nostr
        .public_hex())
}

/// Check that a backup (`method`: "passkey", "phrase" or "state") would
/// restore this wallet, changing nothing. Rejects with `{ code, message }`.
#[tauri::command]
pub async fn test_backup(
    app: AppHandle,
    state: State<'_, AppState>,
    method: String,
    phrase: Option<String>,
) -> Result<BackupTest, TestFailure> {
    let phrase = phrase.map(Zeroizing::new);
    let method = Method::parse(&method)?;
    let b = &state.backup;
    if !b.is_online() {
        return Err(failure(TestCode::Failed, "offline"));
    }
    let (keys, network, origin, credential) = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        (
            StateKeys::from_master(&u.seed.master_xpriv(u.config.network)?)?,
            u.config.network,
            u.config.backup.seed_origin,
            u.config.backup.credential_id.clone(),
        )
    };
    let me = keys.nostr.public_hex();

    let seed = match method {
        Method::State => None,
        Method::Phrase => {
            let typed = phrase
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| failure(TestCode::InvalidPhrase, "Enter your recovery phrase"))?;
            let seed = Seed::from_phrase(&typed).map_err(|_| {
                failure(
                    TestCode::InvalidPhrase,
                    "That isn't a valid recovery phrase. Check each word and try again",
                )
            })?;
            Some(if seed_id(&seed, network)? == me {
                SeedCheck::Matches { envelope: false }
            } else {
                SeedCheck::Different
            })
        }
        Method::Passkey => {
            let got = sign_in(&app).await.map_err(from_passkey)?;
            let pk = PasskeyKeys::from_prf(&got.prf)?;
            let lookup = passkey_seed(&state, &pk).await?;
            let (found, matches) = match &lookup.found {
                PasskeySeed::Envelope { seed, .. } => {
                    (Found::Envelope, seed_id(seed, network)? == me)
                }
                PasskeySeed::Derived(seed) => (Found::Derived, seed_id(seed, network)? == me),
                PasskeySeed::Unreachable => (
                    Found::Unreachable {
                        answered: lookup.answered,
                        total: lookup.total,
                    },
                    false,
                ),
            };
            let same = credential.as_ref().map(|c| *c == got.credential_id);
            Some(passkey_outcome(found, matches, origin, same))
        }
    };

    let local = b.last_version();
    if seed.is_some_and(|s| !matches!(s, SeedCheck::Matches { .. })) {
        return Ok(summarize(method, seed, None, local));
    }
    let (write, read) = b.urls();
    let answers = b.relays().fetch(&read, &me, KIND_APP_DATA).await;
    let checked = check_state(&keys, &answers, &write);
    let result = summarize(method, seed, Some(&checked), local);
    tracing::info!(
        ?method,
        ok = result.ok,
        found = result.state_found,
        relays = result.relays_with_latest,
        "backup: tested"
    );
    if result.ok {
        let mut guard = state.inner.lock().await;
        if let Some(u) = guard.as_mut() {
            u.config.backup.last_tested_at = Some(now_secs());
            // Not `save_config`: nothing to back up.
            if let Err(e) = u.config.save(&state.paths.config()) {
                tracing::warn!("backup: couldn't record the test: {e}");
            }
            b.flags_changed(&u.config);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::{sha256, Hash};

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const LEGAL: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    const RELAYS: [&str; 6] = [
        "wss://a", "wss://b", "wss://c", "wss://d", "wss://e", "wss://f",
    ];

    fn keys(phrase: &str) -> StateKeys {
        StateKeys::from_master(
            &Seed::from_phrase(phrase)
                .unwrap()
                .master_xpriv(Network::Bitcoin)
                .unwrap(),
        )
        .unwrap()
    }

    fn snap(version: u64, labels: usize) -> Snapshot {
        Snapshot {
            version,
            created_at: 1_700_000_000 + version,
            data: serde_json::json!({
                "config": { "accounts": [
                    { "kind": "spending", "id": "spending" },
                    { "kind": "savings", "id": "savings" },
                    { "kind": "hardware", "id": "hw-1" },
                    { "kind": "hardware", "id": "hw-2" },
                    { "kind": "watchOnly", "id": "watch-1" },
                ]},
                "labels": { "savings": vec![serde_json::json!({"type": "tx"}); labels] },
            }),
        }
    }

    fn events(k: &StateKeys, s: &Snapshot) -> Vec<Event> {
        snapshot::encode(k, s, s.created_at).unwrap().unwrap()
    }

    fn urls() -> Vec<String> {
        RELAYS.iter().map(|s| s.to_string()).collect()
    }

    /// Each relay: Some(events) answered, None didn't.
    fn answers(per: Vec<Option<Vec<Event>>>) -> PerRelay<Vec<Event>> {
        RELAYS
            .iter()
            .zip(per)
            .map(|(u, r)| {
                (
                    u.to_string(),
                    r.ok_or_else(|| Error::rail("relay", "timed out")),
                )
            })
            .collect()
    }

    #[test]
    fn same_seed_same_id_whatever_the_spelling() {
        let a = Seed::from_phrase(ABANDON).unwrap();
        let typed = Seed::from_phrase(&format!(
            "  {}  ",
            ABANDON.to_uppercase().replace(' ', "\n  ")
        ))
        .unwrap();
        let other = Seed::from_phrase(LEGAL).unwrap();
        let id = seed_id(&a, Network::Bitcoin).unwrap();
        assert_eq!(id, seed_id(&typed, Network::Bitcoin).unwrap());
        assert_eq!(id, keys(ABANDON).nostr.public_hex());
        assert_ne!(id, seed_id(&other, Network::Bitcoin).unwrap());
        // A 24-word phrase parses (and is a different wallet).
        let w24 = Seed::from_entropy(&[0u8; 32]).unwrap();
        assert_eq!(w24.word_count(), 24);
        assert_ne!(id, seed_id(&w24, Network::Bitcoin).unwrap());
    }

    #[test]
    fn passkey_envelope_vs_derived() {
        use SeedOrigin::*;
        let unreachable = Found::Unreachable {
            answered: 1,
            total: 6,
        };
        // The seed it restores is this wallet's.
        assert_eq!(
            passkey_outcome(Found::Envelope, true, Phrase, Some(true)),
            SeedCheck::Matches { envelope: true }
        );
        assert_eq!(
            passkey_outcome(Found::Derived, true, Passkey, None),
            SeedCheck::Matches { envelope: false }
        );
        // Another wallet's envelope, whichever passkey.
        assert_eq!(
            passkey_outcome(Found::Envelope, false, Phrase, Some(true)),
            SeedCheck::Different
        );
        // A passkey wallet and another passkey.
        assert_eq!(
            passkey_outcome(Found::Derived, false, Passkey, Some(true)),
            SeedCheck::Different
        );
        // A phrase wallet: its own passkey (or one we can't rule out) with
        // no envelope means the phrase backup is gone; a known other one
        // is just the wrong passkey.
        assert_eq!(
            passkey_outcome(Found::Derived, false, Phrase, Some(true)),
            SeedCheck::EnvelopeMissing
        );
        assert_eq!(
            passkey_outcome(Found::Derived, false, Phrase, None),
            SeedCheck::EnvelopeMissing
        );
        assert_eq!(
            passkey_outcome(Found::Derived, false, Phrase, Some(false)),
            SeedCheck::Different
        );
        assert_eq!(
            passkey_outcome(unreachable, false, Phrase, Some(true)),
            SeedCheck::Unreachable {
                answered: 1,
                total: 6
            }
        );
    }

    #[test]
    fn different_seed_stops_before_the_state() {
        let t = summarize(Method::Passkey, Some(SeedCheck::Different), None, Some(9));
        assert!(!t.ok && !t.state_found);
        assert_eq!(t.seed_matches, Some(false));
        assert_eq!(t.message, "That passkey belongs to a different wallet.");

        let t = summarize(
            Method::Passkey,
            Some(SeedCheck::EnvelopeMissing),
            None,
            None,
        );
        assert!(!t.ok);
        assert!(
            t.message.contains("recovery phrase backup isn't on it"),
            "{}",
            t.message
        );

        let t = summarize(Method::Phrase, Some(SeedCheck::Different), None, None);
        assert!(t
            .message
            .starts_with("That recovery phrase is for a different wallet"));

        let t = summarize(
            Method::Passkey,
            Some(SeedCheck::Unreachable {
                answered: 1,
                total: 6,
            }),
            None,
            None,
        );
        assert!(!t.ok && t.seed_matches.is_none());
        assert_eq!((t.relays_answered, t.relays_total), (1, 6));
        assert!(t.message.starts_with("Couldn't reach the backup servers"));
    }

    #[test]
    fn matching_seed_and_partial_relays() {
        let k = keys(ABANDON);
        let latest = events(&k, &snap(7, 143));
        let older = events(&k, &snap(5, 100));
        // a, b, c, d, e have v7, f has v5... and d didn't answer.
        let a = answers(vec![
            Some(latest.clone()),
            Some(latest.clone()),
            Some(latest.clone()),
            None,
            Some(latest.clone()),
            Some(older),
        ]);
        let s = check_state(&k, &a, &urls());
        assert!(s.reachable);
        assert_eq!((s.with_latest, s.answered, s.total), (4, 5, 6));
        let l = s.latest.as_ref().unwrap();
        assert_eq!(
            (l.version, l.accounts, l.hardware, l.labels),
            (7, 5, 2, 143)
        );

        let t = summarize(
            Method::Passkey,
            Some(SeedCheck::Matches { envelope: true }),
            Some(&s),
            Some(7),
        );
        assert!(t.ok && t.up_to_date && t.state_found);
        assert_eq!(t.seed_matches, Some(true));
        assert_eq!((t.state_version, t.local_version), (Some(7), Some(7)));
        assert_eq!(t.backed_up_at, Some(1_700_000_007));
        assert_eq!(
            t.message,
            "Your passkey restores this wallet, with 5 accounts, 2 hardware wallets and 143 labels. 4 of 6 backup servers have the latest copy."
        );
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["relaysWithLatest"], 4);
        assert_eq!(json["hardwareWallets"], 2);
        assert!(json["seedMatches"].as_bool().unwrap());

        // `state`: no seed answer.
        let t = summarize(Method::State, None, Some(&s), Some(7));
        assert!(t.ok && t.seed_matches.is_none());
        assert!(t
            .message
            .starts_with("Your online backup is up to date, with"));
        assert_eq!(
            serde_json::to_value(&t).unwrap()["seedMatches"],
            Value::Null
        );
    }

    #[test]
    fn all_relays_one_relay_and_stale_relays() {
        let k = keys(ABANDON);
        let v = events(&k, &snap(7, 1));
        let s = check_state(&k, &answers(vec![Some(v.clone()); 6]), &urls());
        let t = summarize(Method::State, None, Some(&s), Some(7));
        assert!(t.message.ends_with("with 5 accounts, 2 hardware wallets and 1 label. All 6 backup servers have the latest copy."), "{}", t.message);

        let mut one = vec![Some(vec![]); 6];
        one[2] = Some(v.clone());
        let s = check_state(&k, &answers(one), &urls());
        let t = summarize(Method::State, None, Some(&s), Some(7));
        assert!(t.ok);
        assert!(t
            .message
            .ends_with("1 of 6 backup servers has the latest copy."));

        // This device uploaded v9, the relays only have v7.
        let t = summarize(
            Method::Phrase,
            Some(SeedCheck::Matches { envelope: false }),
            Some(&s),
            Some(9),
        );
        assert!(!t.ok && !t.up_to_date && t.state_found);
        assert!(t.message.contains("older copy"), "{}", t.message);

        // Another device uploaded something newer: still up to date.
        let t = summarize(Method::State, None, Some(&s), Some(3));
        assert!(t.ok && t.up_to_date);
    }

    #[test]
    fn nothing_found_or_unreachable() {
        let k = keys(ABANDON);
        // Another seed's backup is ignored.
        let theirs = events(&keys(LEGAL), &snap(7, 1));
        let s = check_state(&k, &answers(vec![Some(theirs); 6]), &urls());
        assert!(s.reachable && s.latest.is_none());
        let t = summarize(
            Method::Passkey,
            Some(SeedCheck::Matches { envelope: false }),
            Some(&s),
            None,
        );
        assert!(!t.ok && !t.state_found);
        assert_eq!(t.seed_matches, Some(true));
        assert!(
            t.message
                .starts_with("Your passkey restores this wallet's keys, but"),
            "{}",
            t.message
        );

        // One relay answered with nothing: can't say there's no backup.
        let mut per = vec![None; 6];
        per[0] = Some(vec![]);
        let s = check_state(&k, &answers(per), &urls());
        assert!(!s.reachable);
        let t = summarize(Method::State, None, Some(&s), Some(7));
        assert!(!t.ok);
        assert_eq!((t.relays_answered, t.relays_total), (1, 6));
        assert!(t.message.starts_with("Couldn't reach the backup servers"));
    }

    #[test]
    fn chunks_split_across_relays_restore_but_dont_count() {
        let k = keys(ABANDON);
        // Incompressible labels: several chunks.
        let mut s = snap(7, 0);
        s.data["labels"]["savings"] = (0..2400u32)
            .map(|i| serde_json::json!({ "ref": sha256::Hash::hash(&i.to_be_bytes()).to_string() }))
            .collect();
        let all = events(&k, &s);
        assert!(all.len() >= 2);
        // Relay a has chunk 0 only, b the rest: together a restore works,
        // but neither holds a full copy.
        let a = answers(vec![
            Some(all[..1].to_vec()),
            Some(all[1..].to_vec()),
            Some(vec![]),
            Some(vec![]),
            None,
            None,
        ]);
        let c = check_state(&k, &a, &urls());
        assert_eq!(c.latest.as_ref().map(|l| l.version), Some(7));
        assert_eq!((c.with_latest, c.answered), (0, 4));
        // Only the write relays are counted.
        let c = check_state(&k, &a, &urls()[..3]);
        assert_eq!((c.answered, c.total), (3, 3));
    }

    #[test]
    fn labels_left_out_are_mentioned() {
        let l = Latest {
            version: 1,
            created_at: 1,
            accounts: 1,
            hardware: 0,
            labels: 0,
            labels_omitted: true,
        };
        let s = StateCheck {
            reachable: true,
            latest: Some(l),
            with_latest: 3,
            answered: 3,
            total: 6,
        };
        let t = summarize(Method::State, None, Some(&s), None);
        assert!(t.ok);
        assert_eq!(t.message, "Your online backup is up to date, with 1 account. 3 of 6 backup servers have the latest copy. Labels aren't included: there are too many for the online backup.");
    }

    #[test]
    fn errors_have_codes() {
        let e = serde_json::to_value(from_passkey(PasskeyError::Cancelled)).unwrap();
        assert_eq!(e["code"], "cancelled");
        assert_eq!(e["message"], "Passkey cancelled");
        assert_eq!(
            from_passkey(PasskeyError::NoCredential).code,
            TestCode::NoPasskey
        );
        assert_eq!(
            from_passkey(PasskeyError::NotTrusted("assetlinks".into())).code,
            TestCode::Unsupported
        );
        assert_eq!(TestFailure::from(Error::Locked).code, TestCode::Locked);
        assert_eq!(
            serde_json::to_value(TestCode::InvalidPhrase).unwrap(),
            "invalid_phrase"
        );
        assert!(Method::parse("seed").is_err());
    }
}
