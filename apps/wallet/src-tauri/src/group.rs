//! Group vaults: a multisig vault set up and run by several people over an
//! encrypted Marmot group (see `sessions_group` for the protocol and
//! `tauri-plugin-marmot` for the transport).
//!
//! Setup:
//! 1. The creator makes the group and a join code (QR, link or text).
//! 2. People ask to join with the code; the creator approves each one.
//! 3. The creator assigns the vault's slots to members and proposes it.
//! 4. Each member adds their key (this phone, a hardware wallet, a file).
//! 5. The creator finalizes; every member checks address #0 on their own
//!    device and confirms. The vault is added to each member's wallet.
//!
//! What each member's app believes comes from replaying the group's
//! messages through `sessions_group::setup::reduce`, never from one member's
//! say-so.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sessions_core::{AccountInfo, Error, Result};
use sessions_group::join::Joiner;
use sessions_group::msg::{self, Confirm, Finalize, Hello, KeySubmission, Slot, VaultProposal, WalletMsg};
use sessions_group::setup::{self, SetupState};
use tauri::{AppHandle, Manager, State, Wry};
use tauri_plugin_marmot::Marmot;

use crate::config::{AccountSpec, Coordination, KeyHolder, KeySource};
use crate::group_join::{self, CodeView};
use crate::state::AppState;

/// A group vault this wallet takes part in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupVault {
    pub group_id: String,
    pub name: String,
    pub threshold: u8,
    pub total: u8,
    /// We made it (we approve joiners, propose and finalize).
    pub creator: bool,
    /// The vault proposal's id, once sent (or seen).
    #[serde(default)]
    pub proposal: Option<String>,
    /// The wallet account, once the vault is confirmed here.
    #[serde(default)]
    pub account: Option<String>,
    /// Names joiners gave in their requests (creator side).
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    pub created: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    vaults: Vec<GroupVault>,
    /// Our own name, as given when creating or joining.
    #[serde(default)]
    my_name: String,
}

fn path(state: &AppState) -> std::path::PathBuf {
    state.paths.root.join("group-vaults.json")
}

fn load(state: &AppState) -> Store {
    std::fs::read(path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(state: &AppState, s: &Store) -> Result<()> {
    let p = path(state);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(s).map_err(|e| Error::Storage(e.to_string()))?)
        .map_err(|e| Error::Storage(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| Error::Storage(e.to_string()))
}

fn net(e: String) -> Error {
    Error::Invalid(e)
}

fn marmot(app: &AppHandle) -> tauri::State<'_, Marmot<Wry>> {
    app.state::<Marmot<Wry>>()
}

/// Start the group runtime as this wallet (idempotent). Returns our key.
pub(crate) async fn ready(app: &AppHandle, state: &AppState) -> Result<String> {
    let m = marmot(app);
    if !m.available() {
        return Err(Error::Invalid(tauri_plugin_marmot::UNAVAILABLE.into()));
    }
    let key = group_join::wallet_key(state).await?;
    let root = state.paths.root.join("groups");
    std::fs::create_dir_all(&root).map_err(|e| Error::Storage(e.to_string()))?;
    let pubkey = key.public_hex();
    let status = m
        .start(
            &root.to_string_lossy(),
            &group_join::relays(),
            &key.secret_hex(),
            &pubkey,
        )
        .await
        .map_err(net)?;
    if status.account != pubkey {
        return Err(Error::Invalid("Group messaging is signed in as another key".into()));
    }
    Ok(pubkey)
}

async fn send(app: &AppHandle, group: &str, m: &WalletMsg) -> Result<()> {
    marmot(app)
        .send(group, m.kind(), &m.content())
        .await
        .map_err(net)
}

async fn wallet_msgs(app: &AppHandle, group: &str) -> Result<Vec<msg::Msg>> {
    let list = marmot(app)
        .messages(group, &msg::WALLET_KINDS, 2000)
        .await
        .map_err(net)?;
    Ok(list
        .into_iter()
        .map(|m| msg::Msg {
            id: m.id,
            sender: m.sender,
            kind: m.kind,
            content: m.content,
            created_at: m.created_at,
        })
        .collect())
}

fn network(state: &AppState, u: &crate::state::Unlocked) -> sessions_core::Network {
    let _ = state;
    u.config.network
}

fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 40 {
        return Err(Error::Invalid("Add your name (up to 40 characters)".into()));
    }
    Ok(n.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewGroup {
    pub name: String,
    pub threshold: u8,
    pub total: u8,
    /// Our name, shown to the others.
    pub my_name: String,
}

/// Make a group vault: the group, and a join code for the others.
#[tauri::command]
pub async fn group_create(app: AppHandle, state: State<'_, AppState>, req: NewGroup) -> Result<GroupView> {
    state.check_screen()?;
    let name = crate::config::check_name(&req.name)?;
    let my_name = check_name(&req.my_name)?;
    if !(2..=15).contains(&req.total) || req.threshold == 0 || req.threshold > req.total {
        return Err(Error::Invalid("Pick 1 to N signatures from 2 to 15 keys".into()));
    }
    let me = ready(&app, &state).await?;
    let group_id = marmot(&app).create_group(&name).await.map_err(net)?;
    let mut s = load(&state);
    s.my_name = my_name.clone();
    s.vaults.push(GroupVault {
        group_id: group_id.clone(),
        name: name.clone(),
        threshold: req.threshold,
        total: req.total,
        creator: true,
        proposal: None,
        account: None,
        names: BTreeMap::from([(me, my_name.clone())]),
        created: group_join::now(),
    });
    save(&state, &s)?;
    send(&app, &group_id, &WalletMsg::Hello(Hello { name: my_name })).await?;
    group_join::open_code(&state, &group_id, &name).await?;
    view(&app, &state, &group_id).await
}

/// Members, names, the join code and the setup state of a group vault.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    pub vault: GroupVault,
    pub me: String,
    pub members: Vec<MemberView>,
    /// Creator, while people can still join.
    pub code: Option<CodeView>,
    pub setup: Option<SetupState>,
    /// The slots that are ours to fill.
    pub my_slots: Vec<usize>,
    pub short_code: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberView {
    pub pubkey: String,
    pub npub: String,
    pub name: String,
    pub admin: bool,
}

async fn view(app: &AppHandle, state: &AppState, group: &str) -> Result<GroupView> {
    let me = ready(app, state).await?;
    let s = load(state);
    let vault = s
        .vaults
        .iter()
        .find(|v| v.group_id == group)
        .cloned()
        .ok_or_else(|| Error::Invalid("That group vault isn't on this phone".into()))?;
    let members = marmot(app).members(group).await.map_err(net)?;
    let msgs = wallet_msgs(app, group).await?;
    // Names: what each member says about themselves, else what they said
    // when asking to join.
    let mut names = vault.names.clone();
    for m in &msgs {
        if let Ok(WalletMsg::Hello(h)) = WalletMsg::parse(m.kind, &m.content) {
            if !h.name.trim().is_empty() {
                names.insert(m.sender.clone(), h.name.chars().take(40).collect());
            }
        }
    }
    let network = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        network(state, u)
    };
    let proposal_id = vault.proposal.clone().or_else(|| {
        msgs.iter().find_map(|m| match WalletMsg::parse(m.kind, &m.content) {
            Ok(WalletMsg::Proposal(p)) => Some(p.id),
            _ => None,
        })
    });
    let setup = proposal_id
        .as_deref()
        .and_then(|id| setup::reduce(&msgs, id, network));
    let my_slots = setup
        .as_ref()
        .map(|s| {
            s.slots
                .iter()
                .enumerate()
                .filter(|(_, x)| x.holder == me)
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default();
    let code = if vault.creator && setup.is_none() {
        let js = group_join::load(state);
        match js.codes.iter().find(|c| c.draft == group) {
            Some(c) => sessions_group::join::JoinCode::parse(&c.code).ok().map(|code| CodeView {
                code: code.display(),
                link: code.link(),
                expires: c.expires,
            }),
            None => None,
        }
    } else {
        None
    };
    let short_code = setup.as_ref().and_then(|s| s.short_code());
    Ok(GroupView {
        members: members
            .members
            .iter()
            .map(|p| MemberView {
                npub: crate::nostr_id::npub(p).unwrap_or_default(),
                name: names.get(p).cloned().unwrap_or_else(|| "Someone".into()),
                admin: members.admins.contains(p),
                pubkey: p.clone(),
            })
            .collect(),
        me,
        vault,
        code,
        setup,
        my_slots,
        short_code,
    })
}

#[tauri::command]
pub async fn group_view(app: AppHandle, state: State<'_, AppState>, group_id: String) -> Result<GroupView> {
    state.check_screen()?;
    view(&app, &state, &group_id).await
}

/// Creator: a new join code (the old one expired, or was shared too widely).
#[tauri::command]
pub async fn group_code(app: AppHandle, state: State<'_, AppState>, group_id: String) -> Result<GroupView> {
    state.check_screen()?;
    let s = load(&state);
    let v = s
        .vaults
        .iter()
        .find(|v| v.group_id == group_id && v.creator)
        .ok_or_else(|| Error::Invalid("Only the vault's creator can invite".into()))?;
    group_join::close_code(&state, &group_id)?;
    group_join::open_code(&state, &group_id, &v.name).await?;
    view(&app, &state, &group_id).await
}

/// Creator: people asking to join with the code.
#[tauri::command]
pub async fn group_requests(app: AppHandle, state: State<'_, AppState>, group_id: String) -> Result<Vec<Joiner>> {
    state.check_screen()?;
    ready(&app, &state).await?;
    let members = marmot(&app).members(&group_id).await.map_err(net)?;
    group_join::requests(&state, &group_id, &members.members).await
}

/// Creator: let someone in.
#[tauri::command]
pub async fn group_approve(
    app: AppHandle,
    state: State<'_, AppState>,
    group_id: String,
    joiner: Joiner,
) -> Result<GroupView> {
    state.check_screen()?;
    ready(&app, &state).await?;
    let mut s = load(&state);
    let v = s
        .vaults
        .iter_mut()
        .find(|v| v.group_id == group_id && v.creator)
        .ok_or_else(|| Error::Invalid("Only the vault's creator can approve".into()))?;
    if v.proposal.is_some() {
        return Err(Error::Invalid("The vault is already set up; make a new one to add people".into()));
    }
    v.names.insert(joiner.pubkey.clone(), joiner.name.clone());
    save(&state, &s)?;
    marmot(&app)
        .invite(&group_id, std::slice::from_ref(&joiner.pubkey))
        .await
        .map_err(net)?;
    view(&app, &state, &group_id).await
}

#[tauri::command]
pub async fn group_ignore(state: State<'_, AppState>, group_id: String, pubkey: String) -> Result<()> {
    state.check_screen()?;
    group_join::ignore(&state, &group_id, &pubkey)
}

/// A group vault invite waiting for a yes or no.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteView {
    pub group_id: String,
    pub name: String,
    pub from_npub: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupList {
    pub vaults: Vec<GroupVault>,
    pub invites: Vec<InviteView>,
}

/// Our group vaults. Invites we asked for (with a code) are accepted here;
/// invites from people we share a vault with are listed; others are declined.
#[tauri::command]
pub async fn group_list(app: AppHandle, state: State<'_, AppState>) -> Result<GroupList> {
    state.check_screen()?;
    if !marmot(&app).available() {
        return Ok(GroupList { vaults: load(&state).vaults, invites: Vec::new() });
    }
    ready(&app, &state).await?;
    let rows = marmot(&app).groups().await.map_err(net)?;
    let mut s = load(&state);
    let mut js = group_join::load(&state);
    let cosigners: Vec<String> = {
        let guard = state.inner.lock().await;
        let u = guard.as_ref().ok_or(Error::Locked)?;
        u.config
            .accounts
            .iter()
            .filter_map(|a| match a {
                AccountSpec::Multisig { keys, .. } => Some(keys),
                _ => None,
            })
            .flatten()
            .filter_map(|k| k.holder.as_ref())
            .filter_map(|h| crate::nostr_id::parse_pubkey(&h.npub).ok())
            .collect()
    };
    let mut invites = Vec::new();
    let mut changed = false;
    for r in rows {
        if r.pending {
            let from = r.welcomer.clone().unwrap_or_default();
            if let Some(i) = js.requested.iter().position(|q| q.host == from) {
                // We asked to join this one.
                marmot(&app).accept(&r.group_id).await.map_err(net)?;
                let name = js.requested.remove(i).name;
                group_join::store(&state, &js)?;
                if !s.vaults.iter().any(|v| v.group_id == r.group_id) {
                    s.vaults.push(GroupVault {
                        group_id: r.group_id.clone(),
                        name: if r.name.is_empty() { name } else { r.name.clone() },
                        threshold: 0,
                        total: 0,
                        creator: false,
                        proposal: None,
                        account: None,
                        names: BTreeMap::new(),
                        created: group_join::now(),
                    });
                    changed = true;
                }
                if !s.my_name.is_empty() {
                    let _ = send(&app, &r.group_id, &WalletMsg::Hello(Hello { name: s.my_name.clone() })).await;
                }
            } else if cosigners.contains(&from) {
                invites.push(InviteView {
                    group_id: r.group_id.clone(),
                    name: r.name.clone(),
                    from_npub: crate::nostr_id::npub(&from).ok(),
                });
            } else {
                let _ = marmot(&app).decline(&r.group_id).await;
            }
        } else if !s.vaults.iter().any(|v| v.group_id == r.group_id) {
            // A group we're in but didn't record (accepted elsewhere): track it.
            s.vaults.push(GroupVault {
                group_id: r.group_id.clone(),
                name: r.name.clone(),
                threshold: 0,
                total: 0,
                creator: false,
                proposal: None,
                account: None,
                names: BTreeMap::new(),
                created: group_join::now(),
            });
            changed = true;
        }
    }
    if changed {
        save(&state, &s)?;
    }
    Ok(GroupList { vaults: s.vaults, invites })
}

#[tauri::command]
pub async fn group_respond(app: AppHandle, state: State<'_, AppState>, group_id: String, accept: bool) -> Result<()> {
    state.check_screen()?;
    ready(&app, &state).await?;
    if accept {
        marmot(&app).accept(&group_id).await.map_err(net)?;
        let s = load(&state);
        if !s.my_name.is_empty() {
            send(&app, &group_id, &WalletMsg::Hello(Hello { name: s.my_name })).await?;
        }
    } else {
        marmot(&app).decline(&group_id).await.map_err(net)?;
    }
    Ok(())
}

pub(crate) fn remember_name(state: &AppState, name: &str) -> Result<()> {
    let mut s = load(state);
    s.my_name = check_name(name)?;
    save(state, &s)
}

/// Remember our name for the vaults we join (sent to the group on joining).
#[tauri::command]
pub async fn group_set_name(state: State<'_, AppState>, name: String) -> Result<()> {
    let mut s = load(&state);
    s.my_name = check_name(&name)?;
    save(&state, &s)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSlot {
    pub holder: String,
    pub label: String,
}

/// Creator: who fills which slot. Closes the join code.
#[tauri::command]
pub async fn group_propose(
    app: AppHandle,
    state: State<'_, AppState>,
    group_id: String,
    slots: Vec<NewSlot>,
) -> Result<GroupView> {
    state.check_screen()?;
    ready(&app, &state).await?;
    let network = {
        let guard = state.inner.lock().await;
        guard.as_ref().ok_or(Error::Locked)?.config.network
    };
    let mut s = load(&state);
    let v = s
        .vaults
        .iter_mut()
        .find(|v| v.group_id == group_id && v.creator)
        .ok_or_else(|| Error::Invalid("Only the vault's creator can set it up".into()))?;
    if v.proposal.is_some() {
        return Err(Error::Invalid("This vault is already set up".into()));
    }
    let members = marmot(&app).members(&group_id).await.map_err(net)?;
    if slots.len() != v.total as usize {
        return Err(Error::Invalid(format!("Assign all {} keys", v.total)));
    }
    if let Some(s) = slots.iter().find(|s| !members.members.contains(&s.holder)) {
        return Err(Error::Invalid(format!("\"{}\" isn't in the group", s.label)));
    }
    let p = VaultProposal {
        id: msg::new_id(),
        name: v.name.clone(),
        threshold: v.threshold,
        slots: slots
            .iter()
            .enumerate()
            .map(|(i, s)| Slot {
                holder: s.holder.clone(),
                label: {
                    let l: String = s.label.trim().chars().take(32).collect();
                    if l.is_empty() { format!("Key {}", i + 1) } else { l }
                },
            })
            .collect(),
        network: setup::network_name(network).into(),
    };
    setup::check_proposal(&p).map_err(Error::Invalid)?;
    v.proposal = Some(p.id.clone());
    save(&state, &s)?;
    send(&app, &group_id, &WalletMsg::Proposal(p)).await?;
    group_join::close_code(&state, &group_id)?;
    view(&app, &state, &group_id).await
}

/// Fill one of our slots with a key (from the key picker).
#[tauri::command]
pub async fn group_submit_key(
    app: AppHandle,
    state: State<'_, AppState>,
    group_id: String,
    slot: usize,
    key: String,
    device: Option<String>,
) -> Result<GroupView> {
    state.check_screen()?;
    let current = view(&app, &state, &group_id).await?;
    let setup = current
        .setup
        .as_ref()
        .ok_or_else(|| Error::Invalid("The creator hasn't set up the keys yet".into()))?;
    if !current.my_slots.contains(&slot) {
        return Err(Error::Invalid("That key slot isn't yours".into()));
    }
    if setup.finalized {
        return Err(Error::Invalid("The vault is already finalized".into()));
    }
    let network = {
        let guard = state.inner.lock().await;
        guard.as_ref().ok_or(Error::Locked)?.config.network
    };
    let c = rail_onchain::multisig::Cosigner::parse(&key, network)?;
    if setup
        .slots
        .iter()
        .enumerate()
        .any(|(i, s)| i != slot && s.fingerprint.as_deref() == Some(c.fingerprint.as_str()))
    {
        return Err(Error::Invalid("That key is already in this vault. Each key needs its own device or seed".into()));
    }
    send(
        &app,
        &group_id,
        &WalletMsg::Key(KeySubmission {
            proposal: setup.proposal.id.clone(),
            slot,
            key: c.key_expression(),
            device,
        }),
    )
    .await?;
    view(&app, &state, &group_id).await
}

/// Creator: lock in the vault once every key is in.
#[tauri::command]
pub async fn group_finalize(app: AppHandle, state: State<'_, AppState>, group_id: String) -> Result<GroupView> {
    state.check_screen()?;
    let current = view(&app, &state, &group_id).await?;
    if !current.vault.creator {
        return Err(Error::Invalid("Only the vault's creator can finalize".into()));
    }
    let setup = current
        .setup
        .as_ref()
        .ok_or_else(|| Error::Invalid("Set up the keys first".into()))?;
    if setup.finalized {
        return Ok(current);
    }
    let (Some(descriptor), Some(hash)) = (setup.descriptor.clone(), setup.hash.clone()) else {
        return Err(Error::Invalid("Not every key is in yet".into()));
    };
    send(
        &app,
        &group_id,
        &WalletMsg::Finalize(Finalize {
            proposal: setup.proposal.id.clone(),
            descriptor,
            hash,
        }),
    )
    .await?;
    view(&app, &state, &group_id).await
}

/// "I checked address #0 on my device": add the vault to this wallet and
/// tell the group.
#[tauri::command]
pub async fn group_confirm(app: AppHandle, state: State<'_, AppState>, group_id: String) -> Result<AccountInfo> {
    state.check_screen()?;
    let current = view(&app, &state, &group_id).await?;
    let setup = current
        .setup
        .clone()
        .filter(|s| s.finalized)
        .ok_or_else(|| Error::Invalid("The vault isn't finalized yet".into()))?;
    let names: BTreeMap<String, String> = current
        .members
        .iter()
        .map(|m| (m.pubkey.clone(), m.name.clone()))
        .collect();

    let mut s = load(&state);
    let existing = s
        .vaults
        .iter()
        .find(|v| v.group_id == group_id)
        .and_then(|v| v.account.clone());
    let id = match existing {
        Some(id) => sessions_core::AccountId(id),
        None => {
            let spec = {
                let mut guard = state.inner.lock().await;
                let u = guard.as_mut().ok_or(Error::Locked)?;
                let mut spec = crate::vault::build(
                    u,
                    crate::vault::NewVault {
                        name: setup.proposal.name.clone(),
                        threshold: setup.proposal.threshold,
                        keys: setup
                            .slots
                            .iter()
                            .map(|x| crate::vault::NewKey {
                                key: x.key.clone().unwrap_or_default(),
                                label: x.label.clone(),
                                source: KeySource::External,
                            })
                            .collect(),
                    },
                )?;
                if let AccountSpec::Multisig { keys, coordination, .. } = &mut spec {
                    *coordination = Coordination::Group { group_id: group_id.clone() };
                    for (k, slot) in keys.iter_mut().zip(&setup.slots) {
                        k.holder = Some(KeyHolder {
                            npub: crate::nostr_id::npub(&slot.holder)?,
                            name: names.get(&slot.holder).cloned(),
                        });
                    }
                }
                spec
            };
            let id = state.add_account(spec).await?;
            if let Some(v) = s.vaults.iter_mut().find(|v| v.group_id == group_id) {
                v.account = Some(id.to_string());
                v.proposal = Some(setup.proposal.id.clone());
                v.threshold = setup.proposal.threshold;
                v.total = setup.slots.len() as u8;
            }
            save(&state, &s)?;
            id
        }
    };
    send(
        &app,
        &group_id,
        &WalletMsg::Confirm(Confirm {
            proposal: setup.proposal.id.clone(),
            hash: setup.hash.clone().unwrap_or_default(),
            address: setup.first_address.clone().unwrap_or_default(),
        }),
    )
    .await?;
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

/// Account ids of the vaults run as group vaults (for their icon).
#[tauri::command]
pub async fn group_accounts(state: State<'_, AppState>) -> Result<Vec<String>> {
    let guard = state.inner.lock().await;
    let u = guard.as_ref().ok_or(Error::Locked)?;
    Ok(u.config
        .accounts
        .iter()
        .filter_map(|a| match a {
            AccountSpec::Multisig {
                id,
                coordination: Coordination::Group { .. },
                ..
            } => Some(id.to_string()),
            _ => None,
        })
        .collect())
}
