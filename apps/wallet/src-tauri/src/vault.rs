//! Multisig vaults: creating one from keys (hardware wallets, this phone's
//! own key, keys from files or QR codes), importing one from another
//! coordinator's file, exporting it, key labels, and collecting signatures
//! on a spend ("proposals").
//!
//! Proposals are kept on this phone for now. They're the unit a group
//! wallet will share between members later (over a WhiteNoise / Marmot group
//! on Nostr), so they carry everything needed to finish the spend.

use std::collections::BTreeMap;
use std::str::FromStr;

use bitcoin::psbt::Psbt;
use rail_onchain::descriptors;
use rail_onchain::multisig::{self, Cosigner, MultisigConfig};
use serde::{Deserialize, Serialize};
use sessions_core::{AccountId, AccountInfo, Error, Network, Result};
use tauri::State;

use crate::config::{AccountSpec, Config, Coordination, KeySource, VaultKey};
use crate::state::AppState;

fn bip48_account(path: &str) -> Option<u32> {
    let levels: Vec<&str> = path.trim_start_matches("m/").split('/').collect();
    if levels.len() != 4
        || levels[0].trim_end_matches(['\'', 'h']) != "48"
        || levels[3].trim_end_matches(['\'', 'h']) != "2"
    {
        return None;
    }
    levels[2].trim_end_matches(['\'', 'h']).parse().ok()
}

/// A key as the UI sees it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyView {
    pub fingerprint: String,
    pub path: String,
    pub xpub: String,
    /// `[fp/48h/0h/0h/2h]xpub`.
    pub key: String,
    pub label: String,
    pub source: KeySource,
    /// This phone can sign with it.
    pub ours: bool,
}

/// What we know about a key just read: is it this phone's, or a hardware
/// wallet already in the wallet?
fn recognise(
    c: &Cosigner,
    config: &Config,
    our_fp: &str,
    network: Network,
    master: &bitcoin::bip32::Xpriv,
) -> (KeySource, Option<String>) {
    if c.fingerprint == our_fp {
        if let Some(a) = bip48_account(&c.path) {
            if multisig_key(master, network, a).ok().as_deref() == Some(&c.key_expression()) {
                return (KeySource::Phone { account: a }, Some("This phone".into()));
            }
        }
    }
    for spec in &config.accounts {
        if let AccountSpec::Hardware {
            device,
            fingerprint,
            name,
            ..
        } = spec
        {
            if fingerprint.eq_ignore_ascii_case(&c.fingerprint) {
                return (
                    KeySource::Device {
                        device: device.clone(),
                    },
                    Some(name.clone()),
                );
            }
        }
    }
    (KeySource::External, c.label.clone())
}

fn multisig_key(master: &bitcoin::bip32::Xpriv, network: Network, account: u32) -> Result<String> {
    let k = descriptors::key_expression(master, &multisig::bip48_path(network, account))?;
    Ok(Cosigner::parse(&k, network)?.key_expression())
}

/// The vault's keys, labelled (older vaults get labels from the descriptor).
pub fn vault_keys(
    spec: &AccountSpec,
    network: Network,
    our_fp: &str,
) -> Result<(MultisigConfig, Vec<VaultKey>)> {
    let AccountSpec::Multisig {
        descriptors: d,
        keys,
        local_account,
        ..
    } = spec
    else {
        return Err(Error::Invalid("That account isn't a multisig vault".into()));
    };
    let config = MultisigConfig::from_descriptor(&d.external, network)?;
    if !keys.is_empty() {
        return Ok((config, keys.clone()));
    }
    let keys = config
        .cosigners
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mine = c.fingerprint == our_fp
                && local_account.is_some()
                && bip48_account(&c.path) == *local_account;
            VaultKey {
                fingerprint: c.fingerprint.clone(),
                path: c.path.clone(),
                xpub: c.xpub.clone(),
                label: if mine {
                    "This phone".into()
                } else {
                    format!("Key {}", i + 1)
                },
                source: if mine {
                    KeySource::Phone {
                        account: local_account.unwrap_or(0),
                    }
                } else {
                    KeySource::External
                },
                holder: None,
            }
        })
        .collect();
    Ok((config, keys))
}

fn view(k: &VaultKey) -> KeyView {
    let c = Cosigner {
        fingerprint: k.fingerprint.clone(),
        path: k.path.clone(),
        xpub: k.xpub.clone(),
        label: None,
    };
    KeyView {
        fingerprint: k.fingerprint.clone(),
        path: k.path.clone(),
        xpub: k.xpub.clone(),
        key: c.key_expression(),
        label: k.label.clone(),
        source: k.source.clone(),
        ours: matches!(k.source, KeySource::Phone { .. }),
    }
}

/// This phone's key for a new vault (the next unused BIP48 account).
#[tauri::command]
pub async fn vault_phone_key(state: State<'_, AppState>) -> Result<KeyView> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let network = u.config.network;
    let master = u.seed.master_xpriv(network)?;
    let account = u.config.next_multisig_account;
    let c = Cosigner::parse(&multisig_key(&master, network, account)?, network)?;
    Ok(view(&VaultKey {
        fingerprint: c.fingerprint,
        path: c.path,
        xpub: c.xpub,
        label: "This phone".into(),
        source: KeySource::Phone { account },
        holder: None,
    }))
}

/// A key from whatever was scanned, pasted or opened: a key expression, a
/// device's multisig export file.
#[tauri::command]
pub async fn vault_read_key(state: State<'_, AppState>, text: String) -> Result<KeyView> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let network = u.config.network;
    let c = multisig::cosigner_from_export(&text, network)?;
    let master = u.seed.master_xpriv(network)?;
    let our_fp = master
        .fingerprint(&bitcoin::secp256k1::Secp256k1::new())
        .to_string();
    let (source, label) = recognise(&c, &u.config, &our_fp, network, &master);
    Ok(view(&VaultKey {
        fingerprint: c.fingerprint.clone(),
        path: c.path.clone(),
        xpub: c.xpub.clone(),
        label: label.unwrap_or_default(),
        source,
        holder: None,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewKey {
    /// `[fp/path]xpub`.
    pub key: String,
    pub label: String,
    pub source: KeySource,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewVault {
    pub name: String,
    pub threshold: u8,
    pub keys: Vec<NewKey>,
}

fn clean_label(label: &str, i: usize) -> String {
    let l = label.trim();
    if l.is_empty() {
        format!("Key {}", i + 1)
    } else {
        l.chars().take(32).collect()
    }
}

/// Build the vault spec from keys: checks the quorum, works out which key is
/// this phone's.
fn build(u: &mut crate::state::Unlocked, v: NewVault) -> Result<AccountSpec> {
    let network = u.config.network;
    let master = u.seed.master_xpriv(network)?;
    let our_fp = master
        .fingerprint(&bitcoin::secp256k1::Secp256k1::new())
        .to_string();
    let mut cosigners = Vec::new();
    let mut keys = Vec::new();
    let mut local_account = None;
    for (i, k) in v.keys.iter().enumerate() {
        let c = Cosigner::parse(&k.key, network)?;
        let mut source = k.source.clone();
        // Trust the key, not the label: a phone key must be ours.
        let (seen, _) = recognise(&c, &u.config, &our_fp, network, &master);
        match (&source, &seen) {
            (KeySource::Phone { .. }, KeySource::Phone { account }) => {
                if local_account.is_some() {
                    return Err(Error::Invalid(
                        "This phone's key is in the vault twice".into(),
                    ));
                }
                local_account = Some(*account);
                source = seen.clone();
            }
            (KeySource::Phone { .. }, _) => {
                return Err(Error::Invalid("That key isn't this phone's".into()));
            }
            (_, KeySource::Phone { account }) => {
                // Added from a file but it's ours after all: we can sign with it.
                local_account = Some(*account);
                source = seen.clone();
            }
            _ => {}
        }
        keys.push(VaultKey {
            fingerprint: c.fingerprint.clone(),
            path: c.path.clone(),
            xpub: c.xpub.clone(),
            label: clean_label(&k.label, i),
            source,
            holder: None,
        });
        cosigners.push(c);
    }
    let fps: std::collections::HashSet<_> =
        cosigners.iter().map(|c| c.fingerprint.clone()).collect();
    if fps.len() != cosigners.len() {
        return Err(Error::Invalid(
            "Two keys come from the same seed. Each key needs its own device or seed, or one loss could cost you two keys".into(),
        ));
    }
    let config = MultisigConfig::sorted_p2wsh(v.threshold, cosigners);
    config.validate(network)?;
    let descriptors = config.descriptors()?;
    if let Some(a) = local_account {
        u.config.next_multisig_account = u.config.next_multisig_account.max(a + 1);
    }
    let name = if v.name.trim().is_empty() {
        "Vault".to_string()
    } else {
        crate::config::check_name(&v.name)?
    };
    Ok(AccountSpec::Multisig {
        id: u.config.fresh_id("vault"),
        name,
        threshold: v.threshold,
        total: config.total(),
        descriptors,
        local_account,
        keys,
        coordination: Coordination::Solo,
    })
}

#[tauri::command]
pub async fn vault_create(state: State<'_, AppState>, vault: NewVault) -> Result<AccountInfo> {
    state.check_screen()?;
    let spec = {
        let mut guard = state.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        build(u, vault)?
    };
    let id = state.add_account(spec).await?;
    let wallet = state
        .inner
        .lock()
        .await
        .as_ref()
        .ok_or(Error::Locked)?
        .wallet
        .clone();
    wallet.account(&id).await?.info().await
}

/// A multisig wallet file or QR, read for review before it's added.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub name: String,
    pub threshold: u8,
    pub total: u8,
    /// "Native SegWit (P2WSH)" etc.
    pub script: String,
    pub keys: Vec<KeyView>,
    /// Only P2WSH sortedmulti vaults can be created here; others import as
    /// they are.
    pub notes: Vec<String>,
    /// The descriptor, to create the vault exactly as the file says.
    pub descriptor: String,
}

#[tauri::command]
pub async fn vault_import_preview(
    state: State<'_, AppState>,
    text: String,
) -> Result<ImportPreview> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let network = u.config.network;
    let config = MultisigConfig::import(&text, network)?;
    let master = u.seed.master_xpriv(network)?;
    let our_fp = master
        .fingerprint(&bitcoin::secp256k1::Secp256k1::new())
        .to_string();
    let keys = config
        .cosigners
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (source, label) = recognise(c, &u.config, &our_fp, network, &master);
            view(&VaultKey {
                fingerprint: c.fingerprint.clone(),
                path: c.path.clone(),
                xpub: c.xpub.clone(),
                label: c
                    .label
                    .clone()
                    .or(label)
                    .unwrap_or_else(|| format!("Key {}", i + 1)),
                source,
                holder: None,
            })
        })
        .collect::<Vec<_>>();
    let mut notes = Vec::new();
    if !keys.iter().any(|k| k.ours) {
        notes.push("None of these keys is this phone's: you'll sign with your hardware wallets or other cosigners.".into());
    }
    let multisig::Policy::Multi { sorted, .. } = config.policy;
    if !sorted {
        notes.push("This vault keeps its keys in a fixed order (multi, not sortedmulti). That's fine; it's kept as it is.".into());
    }
    Ok(ImportPreview {
        name: config.name.clone().unwrap_or_else(|| "Vault".into()),
        threshold: config.threshold(),
        total: config.total(),
        script: config.script.label().into(),
        keys,
        notes,
        descriptor: descriptors::multipath(&config.descriptors()?),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportVault {
    pub name: String,
    pub descriptor: String,
    /// Labels by fingerprint (and the source the user picked).
    pub keys: Vec<ImportKey>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportKey {
    pub fingerprint: String,
    pub label: String,
    pub source: KeySource,
}

/// Add a vault exactly as an imported file describes it (any script type,
/// sorted or not).
#[tauri::command]
pub async fn vault_import(state: State<'_, AppState>, vault: ImportVault) -> Result<AccountInfo> {
    state.check_screen()?;
    let spec = {
        let mut guard = state.inner.lock().await;
        let u = guard.as_mut().ok_or(Error::Locked)?;
        let network = u.config.network;
        let config = MultisigConfig::from_descriptor(&vault.descriptor, network)?;
        let master = u.seed.master_xpriv(network)?;
        let our_fp = master
            .fingerprint(&bitcoin::secp256k1::Secp256k1::new())
            .to_string();
        let mut local_account = None;
        let keys = config
            .cosigners
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let (seen, _) = recognise(c, &u.config, &our_fp, network, &master);
                let picked = vault.keys.iter().find(|k| k.fingerprint == c.fingerprint);
                let source = match (&seen, picked) {
                    (KeySource::Phone { account }, _) => {
                        local_account = Some(*account);
                        seen.clone()
                    }
                    (_, Some(p)) if !matches!(p.source, KeySource::Phone { .. }) => {
                        p.source.clone()
                    }
                    _ => seen.clone(),
                };
                VaultKey {
                    fingerprint: c.fingerprint.clone(),
                    path: c.path.clone(),
                    xpub: c.xpub.clone(),
                    label: clean_label(picked.map(|p| p.label.as_str()).unwrap_or(""), i),
                    source,
                    holder: None,
                }
            })
            .collect();
        if let Some(a) = local_account {
            u.config.next_multisig_account = u.config.next_multisig_account.max(a + 1);
        }
        let name = if vault.name.trim().is_empty() {
            "Vault".to_string()
        } else {
            crate::config::check_name(&vault.name)?
        };
        AccountSpec::Multisig {
            id: u.config.fresh_id("vault"),
            name,
            threshold: config.threshold(),
            total: config.total(),
            descriptors: config.descriptors()?,
            local_account,
            keys,
            coordination: Coordination::Solo,
        }
    };
    let id = state.add_account(spec).await?;
    let wallet = state
        .inner
        .lock()
        .await
        .as_ref()
        .ok_or(Error::Locked)?
        .wallet
        .clone();
    wallet.account(&id).await?.info().await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultDetails {
    pub name: String,
    pub threshold: u8,
    pub total: u8,
    pub script: String,
    pub keys: Vec<KeyView>,
    pub coordination: Coordination,
    /// `<0;1>` multipath with checksum, as Sparrow shows it.
    pub descriptor: String,
    pub first_address: String,
}

fn find<'a>(config: &'a Config, id: &AccountId) -> Result<&'a AccountSpec> {
    config
        .accounts
        .iter()
        .find(|a| a.id() == id && matches!(a, AccountSpec::Multisig { .. }))
        .ok_or_else(|| Error::Invalid("That vault is gone".into()))
}

fn our_fingerprint(u: &crate::state::Unlocked) -> Result<String> {
    let master = u.seed.master_xpriv(u.config.network)?;
    Ok(master
        .fingerprint(&bitcoin::secp256k1::Secp256k1::new())
        .to_string())
}

#[tauri::command]
pub async fn vault_details(state: State<'_, AppState>, account: AccountId) -> Result<VaultDetails> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let network = u.config.network;
    let spec = find(&u.config, &account)?;
    let (config, keys) = vault_keys(spec, network, &our_fingerprint(u)?)?;
    let AccountSpec::Multisig {
        name, coordination, ..
    } = spec
    else {
        unreachable!("find returns vaults");
    };
    Ok(VaultDetails {
        name: name.clone(),
        threshold: config.threshold(),
        total: config.total(),
        script: config.script.label().into(),
        keys: keys.iter().map(view).collect(),
        coordination: coordination.clone(),
        descriptor: config.descriptor_with_checksum()?,
        first_address: config.first_address(network)?,
    })
}

#[tauri::command]
pub async fn vault_set_key_label(
    state: State<'_, AppState>,
    account: AccountId,
    fingerprint: String,
    label: String,
) -> Result<()> {
    state.check_screen()?;
    let mut guard = state.inner.lock().await;
    let u = guard.as_mut().ok_or(Error::Locked)?;
    let network = u.config.network;
    let our_fp = our_fingerprint(u)?;
    let spec = find(&u.config, &account)?.clone();
    let (_, mut keys) = vault_keys(&spec, network, &our_fp)?;
    let k = keys
        .iter_mut()
        .find(|k| k.fingerprint == fingerprint)
        .ok_or_else(|| Error::Invalid("That key isn't in this vault".into()))?;
    let l = label.trim();
    if l.is_empty() || l.chars().count() > 32 {
        return Err(Error::Invalid("Use a name of 1 to 32 characters".into()));
    }
    k.label = l.to_string();
    for a in &mut u.config.accounts {
        if a.id() == &account {
            if let AccountSpec::Multisig { keys: stored, .. } = a {
                *stored = keys;
            }
            break;
        }
    }
    state.save_config(&u.config)
}

/// The vault as a file another wallet opens.
#[tauri::command]
pub async fn vault_export(
    state: State<'_, AppState>,
    account: AccountId,
    format: multisig::ExportFormat,
) -> Result<multisig::ExportFile> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let network = u.config.network;
    let spec = find(&u.config, &account)?;
    let (mut config, keys) = vault_keys(spec, network, &our_fingerprint(u)?)?;
    config.name = Some(match spec {
        AccountSpec::Multisig { name, .. } => name.clone(),
        _ => "Vault".into(),
    });
    for c in &mut config.cosigners {
        c.label = keys
            .iter()
            .find(|k| k.fingerprint == c.fingerprint)
            .map(|k| k.label.clone());
    }
    config.export(format, network)
}

// ---- Signatures on a spend ------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerStatus {
    pub fingerprint: String,
    pub label: String,
    pub source: KeySource,
    pub signed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signatures {
    pub keys: Vec<SignerStatus>,
    pub signed: u8,
    pub needed: u8,
    /// Enough signatures to send.
    pub ready: bool,
    /// Where the money goes (outputs that aren't the vault's change).
    pub destinations: Vec<Destination>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    pub address: String,
    pub amount_sat: u64,
}

/// Which keys have signed every input of `psbt`.
fn signed_by(psbt: &Psbt) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for input in &psbt.inputs {
        let finalized = input.final_script_witness.is_some() || input.final_script_sig.is_some();
        let mut here: BTreeMap<String, bool> = BTreeMap::new();
        for (pk, (fp, _)) in &input.bip32_derivation {
            let key = bitcoin::PublicKey::new(*pk);
            let has = finalized || input.partial_sigs.contains_key(&key);
            here.insert(fp.to_string(), has);
        }
        for (fp, has) in here {
            let e = out.entry(fp).or_insert(true);
            *e &= has;
        }
    }
    out
}

#[tauri::command]
pub async fn vault_signatures(
    state: State<'_, AppState>,
    account: AccountId,
    psbt: String,
) -> Result<Signatures> {
    state.check_screen()?;
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    let spec = find(&u.config, &account)?;
    let (config, keys) = vault_keys(spec, u.config.network, &our_fingerprint(u)?)?;
    let psbt =
        Psbt::from_str(psbt.trim()).map_err(|_| Error::Invalid("That isn't a PSBT".into()))?;
    let by = signed_by(&psbt);
    let keys: Vec<SignerStatus> = keys
        .iter()
        .map(|k| SignerStatus {
            fingerprint: k.fingerprint.clone(),
            label: k.label.clone(),
            source: k.source.clone(),
            signed: by.get(&k.fingerprint).copied().unwrap_or(false),
        })
        .collect();
    let signed = keys.iter().filter(|k| k.signed).count() as u8;
    let needed = config.threshold();
    let destinations = psbt
        .unsigned_tx
        .output
        .iter()
        .zip(&psbt.outputs)
        .filter(|(_, o)| o.bip32_derivation.is_empty() && o.witness_script.is_none())
        .map(|(txo, _)| Destination {
            address: bitcoin::Address::from_script(
                &txo.script_pubkey,
                u.config.network.to_bitcoin(),
            )
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "an unusual output".into()),
            amount_sat: txo.value.to_sat(),
        })
        .collect();
    Ok(Signatures {
        destinations,
        ready: signed >= needed,
        keys,
        signed,
        needed,
    })
}

/// Fold a cosigner's signatures into the running PSBT.
#[tauri::command]
pub async fn vault_combine(psbt: String, other: String) -> Result<String> {
    let mut a =
        Psbt::from_str(psbt.trim()).map_err(|_| Error::Invalid("That isn't a PSBT".into()))?;
    let b = Psbt::from_str(other.trim())
        .map_err(|_| Error::Invalid("That isn't a signed transaction (PSBT)".into()))?;
    if a.unsigned_tx.compute_txid() != b.unsigned_tx.compute_txid() {
        return Err(Error::Invalid(
            "That's a different transaction. Sign this one".into(),
        ));
    }
    a.combine(b)
        .map_err(|e| Error::Invalid(format!("Couldn't add those signatures: {e}")))?;
    Ok(a.to_string())
}

// ---- Proposals: spends waiting for signatures ---------------------------

/// A spend from a vault that's waiting for signatures. Self-contained, so it
/// can be finished later, and (later) shared with a group's members.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub id: String,
    pub account: AccountId,
    pub created_at: u64,
    pub updated_at: u64,
    /// The PSBT with every signature so far.
    pub psbt: String,
    /// The paused plan (`Execution`) and fee choice, as the UI resumes them.
    pub execution: serde_json::Value,
    pub fee: serde_json::Value,
    pub amount_sat: u64,
    pub fee_sat: u64,
    /// Where it goes, in words ("bc1q…", "Savings").
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub note: Option<String>,
}

fn proposals_path(state: &AppState) -> std::path::PathBuf {
    state.paths.root.join("proposals.json")
}

fn load_proposals(state: &AppState) -> Vec<Proposal> {
    std::fs::read(proposals_path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn store_proposals(state: &AppState, list: &[Proposal]) -> Result<()> {
    let path = proposals_path(state);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(list).map_err(|e| Error::Storage(e.to_string()))?,
    )
    .map_err(|e| Error::Storage(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::Storage(e.to_string()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tauri::command]
pub async fn proposals(
    state: State<'_, AppState>,
    account: Option<AccountId>,
) -> Result<Vec<Proposal>> {
    state.check_screen()?;
    let mut list = load_proposals(&state);
    if let Some(a) = account {
        list.retain(|p| p.account == a);
    }
    list.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
    Ok(list)
}

/// Save (or update, by id) a proposal. An empty id makes a new one.
#[tauri::command]
pub async fn proposal_save(state: State<'_, AppState>, mut proposal: Proposal) -> Result<Proposal> {
    state.check_screen()?;
    let _guard = state.inner.lock().await;
    let mut list = load_proposals(&state);
    if proposal.id.is_empty() {
        proposal.id = format!("p{}", now());
        proposal.created_at = now();
    }
    proposal.updated_at = now();
    list.retain(|p| p.id != proposal.id);
    list.push(proposal.clone());
    store_proposals(&state, &list)?;
    Ok(proposal)
}

#[tauri::command]
pub async fn proposal_delete(state: State<'_, AppState>, id: String) -> Result<()> {
    let _guard = state.inner.lock().await;
    let mut list = load_proposals(&state);
    list.retain(|p| p.id != id);
    store_proposals(&state, &list)
}

#[cfg(test)]
mod tests {
    use super::*;

    use sessions_core::keystore::Seed;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const OTHER: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    const THIRD: &str =
        "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

    fn key_of(phrase: &str, network: Network) -> String {
        let m = Seed::from_phrase(phrase)
            .unwrap()
            .master_xpriv(network)
            .unwrap();
        multisig_key(&m, network, 0).unwrap()
    }

    #[tokio::test]
    async fn vault_from_phone_device_and_outside_keys() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(dir.path().to_path_buf(), None);
        state
            .create(Seed::from_phrase(ABANDON).unwrap(), "123456")
            .await
            .unwrap();
        let mut g = state.inner.lock().await;
        let u = g.as_mut().unwrap();
        let net = u.config.network;
        let phone = key_of(ABANDON, net);
        let k2 = key_of(OTHER, net);
        let k3 = key_of(THIRD, net);
        let key = |k: &str, l: &str, source: KeySource| NewKey {
            key: k.to_string(),
            label: l.to_string(),
            source,
        };
        // A foreign key can't pose as this phone's.
        let bad = NewVault {
            name: "x".into(),
            threshold: 2,
            keys: vec![
                key(&k2, "", KeySource::Phone { account: 0 }),
                key(&k3, "", KeySource::External),
            ],
        };
        assert!(build(u, bad).is_err());
        // The same seed twice is refused.
        let dup = NewVault {
            name: "x".into(),
            threshold: 1,
            keys: vec![
                key(&k2, "", KeySource::External),
                key(&k2, "", KeySource::External),
            ],
        };
        assert!(build(u, dup).is_err());
        // Our own key added as "external" is still recognised as ours.
        let v = NewVault {
            name: "Family".into(),
            threshold: 2,
            keys: vec![
                key(&phone, "", KeySource::External),
                key(
                    &k2,
                    "BitBox",
                    KeySource::Device {
                        device: "BitBox02".into(),
                    },
                ),
                key(&k3, "Mum", KeySource::External),
            ],
        };
        let spec = build(u, v).unwrap();
        let AccountSpec::Multisig {
            local_account,
            keys,
            threshold,
            total,
            name,
            ..
        } = &spec
        else {
            panic!("not a vault")
        };
        assert_eq!((*threshold, *total, name.as_str()), (2, 3, "Family"));
        assert_eq!(*local_account, Some(0));
        assert_eq!(u.config.next_multisig_account, 1);
        assert_eq!(keys[0].source, KeySource::Phone { account: 0 });
        assert_eq!(keys[0].label, "Key 1");
        assert_eq!(keys[1].label, "BitBox");
        // Labels come back, and the descriptor round-trips through the files.
        let our_fp = our_fingerprint(u).unwrap();
        let (config, back) = vault_keys(&spec, net, &our_fp).unwrap();
        assert_eq!(back, *keys);
        let file = config
            .export(multisig::ExportFormat::Coldcard, net)
            .unwrap();
        let again = MultisigConfig::import(&file.content, net).unwrap();
        assert_eq!(
            again.descriptors().unwrap().external.len(),
            config.descriptors().unwrap().external.len()
        );
        drop(g);
        state.add_account(spec.clone()).await.unwrap();
        // Adding the same vault again is refused.
        let mut g = state.inner.lock().await;
        let u = g.as_mut().unwrap();
        let again = NewVault {
            name: "Again".into(),
            threshold: 2,
            keys: vec![
                key(&phone, "", KeySource::Phone { account: 0 }),
                key(&k2, "", KeySource::External),
                key(&k3, "", KeySource::External),
            ],
        };
        let spec2 = build(u, again).unwrap();
        drop(g);
        assert!(state.add_account(spec2).await.is_err());
    }

    #[test]
    fn bip48_accounts() {
        assert_eq!(bip48_account("m/48'/0'/3'/2'"), Some(3));
        assert_eq!(bip48_account("m/48h/1h/0h/2h"), Some(0));
        assert_eq!(bip48_account("m/84'/0'/0'"), None);
        assert_eq!(bip48_account("m/48'/0'/0'/1'"), None);
    }
}

/// Save (system save-as screen, or Downloads on desktop) or share a file.
/// `false`: the user backed out.
#[tauri::command]
pub async fn file_export(
    app: tauri::AppHandle,
    filename: String,
    mime: String,
    content: String,
    share: bool,
) -> Result<bool> {
    use tauri::Manager;
    let files = app.state::<tauri_plugin_files::Files<tauri::Wry>>();
    let r = if share {
        files.share(&filename, &mime, &content).await
    } else {
        files.save(&filename, &mime, &content).await
    };
    r.map_err(Error::Invalid)
}
