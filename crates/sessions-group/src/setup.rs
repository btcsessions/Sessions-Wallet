//! A vault's setup, worked out from the group's messages.
//!
//! Rules (every member's app applies the same ones, so they agree):
//!
//! * The proposal is the first one with that id; its sender is the proposer.
//! * A slot is filled only by its holder. A later key replaces an earlier one,
//!   until the vault is finalized.
//! * Only the proposer finalizes, and only with exactly the descriptor the
//!   submitted keys make. Anything else is ignored (and reported).
//! * A confirm counts only from a slot holder, for the finalized hash.
//! * The vault is ready when every holder has confirmed.

use std::collections::BTreeSet;

use bitcoin::hashes::{sha256, Hash};
use rail_onchain::multisig::{Cosigner, MultisigConfig};
use serde::Serialize;
use sessions_core::Network;

use crate::msg::{Msg, VaultProposal, WalletMsg};

/// The most keys a group vault can have (BIP67 sortedmulti in P2WSH).
pub const MAX_SLOTS: usize = 15;

/// SHA-256 (hex) of the receive descriptor with checksum: the vault's id in
/// messages, and what members compare.
pub fn descriptor_hash(descriptor: &str) -> String {
    sha256::Hash::hash(descriptor.trim().as_bytes()).to_string()
}

/// Sixteen digits from the hash (four groups of four), read aloud on a call
/// to catch a swapped key. Long enough (~53 bits) that whoever submits the
/// last key can't try keys until the code matches one they want.
pub fn short_code(hash: &str) -> String {
    let n = u64::from_str_radix(hash.get(..16).unwrap_or("0"), 16).unwrap_or(0) % 10_000_000_000_000_000;
    let d = format!("{n:016}");
    format!("{} {} {} {}", &d[..4], &d[4..8], &d[8..12], &d[12..])
}

pub fn network_name(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "bitcoin",
        Network::Testnet => "testnet",
        Network::Signet => "signet",
        Network::Regtest => "regtest",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotState {
    pub holder: String,
    pub label: String,
    /// `[fp/path]xpub` once submitted.
    pub key: Option<String>,
    pub fingerprint: Option<String>,
    pub device: Option<String>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    pub proposal: VaultProposal,
    pub proposer: String,
    pub slots: Vec<SlotState>,
    /// Built from the submitted keys once every slot has one.
    pub descriptor: Option<String>,
    pub hash: Option<String>,
    pub first_address: Option<String>,
    pub finalized: bool,
    /// Every holder confirmed address #0 on their own device.
    pub ready: bool,
    /// Messages that were ignored, and why (shown to members).
    pub problems: Vec<String>,
}

impl SetupState {
    pub fn all_keys_in(&self) -> bool {
        self.slots.iter().all(|s| s.key.is_some())
    }

    pub fn short_code(&self) -> Option<String> {
        self.hash.as_deref().map(short_code)
    }
}

/// Check a proposal on its own (before sending, and on receipt).
pub fn check_proposal(p: &VaultProposal) -> Result<(), String> {
    let n = p.slots.len();
    if !(2..=MAX_SLOTS).contains(&n) {
        return Err(format!("A vault needs 2 to {MAX_SLOTS} keys"));
    }
    if p.threshold == 0 || p.threshold as usize > n {
        return Err(format!("Signatures needed must be 1 to {n}"));
    }
    if p.name.trim().is_empty() || p.name.chars().count() > 60 {
        return Err("Name the vault (up to 60 characters)".into());
    }
    if p.slots.iter().any(|s| s.holder.len() != 64 || !s.holder.chars().all(|c| c.is_ascii_hexdigit())) {
        return Err("A slot has no valid holder".into());
    }
    Ok(())
}

fn build(
    threshold: u8,
    keys: &[String],
    network: Network,
) -> Result<(MultisigConfig, String, String), String> {
    let cosigners = keys
        .iter()
        .map(|k| Cosigner::parse(k, network).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let config = MultisigConfig::sorted_p2wsh(threshold, cosigners);
    config.validate(network).map_err(|e| e.to_string())?;
    let descriptor = config.descriptor_with_checksum().map_err(|e| e.to_string())?;
    let address = config.first_address(network).map_err(|e| e.to_string())?;
    Ok((config, descriptor, address))
}

/// The setup state of proposal `id`, from the group's messages in order.
/// `None` when there's no (valid) proposal with that id.
pub fn reduce(msgs: &[Msg], id: &str, network: Network) -> Option<SetupState> {
    let mut state: Option<SetupState> = None;
    let mut confirmed: BTreeSet<String> = BTreeSet::new();
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();

    for m in msgs {
        if !seen_ids.insert(m.id.as_str()) {
            continue;
        }
        let Ok(w) = WalletMsg::parse(m.kind, &m.content) else {
            continue;
        };
        match w {
            WalletMsg::Proposal(p) if p.id == id && state.is_none() => {
                if p.network != network_name(network) {
                    return None;
                }
                if check_proposal(&p).is_err() {
                    return None;
                }
                state = Some(SetupState {
                    slots: p
                        .slots
                        .iter()
                        .map(|s| SlotState {
                            holder: s.holder.clone(),
                            label: s.label.clone(),
                            key: None,
                            fingerprint: None,
                            device: None,
                            confirmed: false,
                        })
                        .collect(),
                    proposal: p,
                    proposer: m.sender.clone(),
                    descriptor: None,
                    hash: None,
                    first_address: None,
                    finalized: false,
                    ready: false,
                    problems: Vec::new(),
                });
            }
            WalletMsg::Key(k) if k.proposal == id => {
                let Some(s) = state.as_mut() else { continue };
                if s.finalized {
                    s.problems.push("A key arrived after the vault was finalized; ignored".into());
                    continue;
                }
                let Some(slot) = s.slots.get(k.slot) else { continue };
                if slot.holder != m.sender {
                    s.problems.push(format!("Someone other than its holder tried to fill \"{}\"; ignored", slot.label));
                    continue;
                }
                let c = match Cosigner::parse(&k.key, network) {
                    Ok(c) => c,
                    Err(e) => {
                        s.problems.push(format!("The key for \"{}\" isn't valid: {e}", slot.label));
                        continue;
                    }
                };
                if s.slots.iter().enumerate().any(|(i, o)| {
                    i != k.slot && o.fingerprint.as_deref() == Some(c.fingerprint.as_str())
                }) {
                    s.problems.push(format!("The key for \"{}\" is already in another slot; ignored", slot.label));
                    continue;
                }
                let slot = &mut s.slots[k.slot];
                slot.key = Some(c.key_expression());
                slot.fingerprint = Some(c.fingerprint.clone());
                slot.device = k.device;
            }
            WalletMsg::Finalize(f) if f.proposal == id => {
                let Some(s) = state.as_mut() else { continue };
                if s.finalized {
                    continue;
                }
                if m.sender != s.proposer {
                    s.problems.push("Someone other than the creator tried to finalize; ignored".into());
                    continue;
                }
                let keys: Option<Vec<String>> = s.slots.iter().map(|x| x.key.clone()).collect();
                let Some(keys) = keys else {
                    s.problems.push("The vault was finalized before every key was in; ignored".into());
                    continue;
                };
                match build(s.proposal.threshold, &keys, network) {
                    Ok((_, descriptor, address)) => {
                        let hash = descriptor_hash(&descriptor);
                        if descriptor_hash(&f.descriptor) != hash || f.hash != hash {
                            s.problems.push("The finalized vault doesn't match the keys submitted; ignored".into());
                            continue;
                        }
                        s.descriptor = Some(descriptor);
                        s.hash = Some(hash);
                        s.first_address = Some(address);
                        s.finalized = true;
                    }
                    Err(e) => s.problems.push(format!("These keys don't make a valid vault: {e}")),
                }
            }
            WalletMsg::Confirm(c) if c.proposal == id => {
                let Some(s) = state.as_mut() else { continue };
                if !s.finalized || s.hash.as_deref() != Some(c.hash.as_str()) {
                    continue;
                }
                if s.first_address.as_deref() != Some(c.address.as_str()) {
                    s.problems.push("A member confirmed a different first address".into());
                    continue;
                }
                if s.slots.iter().any(|x| x.holder == m.sender) {
                    confirmed.insert(m.sender.clone());
                }
            }
            _ => {}
        }
    }

    let mut s = state?;
    // Before finalize, show what the keys so far would make.
    if !s.finalized && s.all_keys_in() {
        let keys: Vec<String> = s.slots.iter().filter_map(|x| x.key.clone()).collect();
        match build(s.proposal.threshold, &keys, network) {
            Ok((_, d, a)) => {
                s.hash = Some(descriptor_hash(&d));
                s.descriptor = Some(d);
                s.first_address = Some(a);
            }
            Err(e) => s.problems.push(format!("These keys don't make a valid vault: {e}")),
        }
    }
    for slot in &mut s.slots {
        slot.confirmed = s.finalized && confirmed.contains(&slot.holder);
    }
    s.ready = s.finalized && s.slots.iter().all(|x| x.confirmed);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::*;

    // Three BIP48 keys (from the rail-onchain multisig tests' seeds would be
    // ideal; any valid xpubs at m/48'/0'/0'/2' do).
    fn keys() -> Vec<String> {
        use bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
        use std::str::FromStr;
        let secp = bitcoin::secp256k1::Secp256k1::new();
        (1u8..=3)
            .map(|i| {
                let m = Xpriv::new_master(bitcoin::NetworkKind::Main, &[i; 32]).unwrap();
                let fp = m.fingerprint(&secp);
                let p = DerivationPath::from_str("m/48'/0'/0'/2'").unwrap();
                let x = Xpub::from_priv(&secp, &m.derive_priv(&secp, &p).unwrap());
                format!("[{fp}/48h/0h/0h/2h]{x}")
            })
            .collect()
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn msg(n: usize, sender: &str, w: WalletMsg) -> Msg {
        Msg {
            id: format!("m{n}"),
            sender: sender.into(),
            kind: w.kind(),
            content: w.content(),
            created_at: n as u64,
        }
    }

    fn proposal() -> WalletMsg {
        WalletMsg::Proposal(VaultProposal {
            id: "p1".into(),
            name: "Family".into(),
            threshold: 2,
            slots: [A, B, C]
                .iter()
                .enumerate()
                .map(|(i, h)| Slot {
                    holder: h.to_string(),
                    label: format!("Key {}", i + 1),
                })
                .collect(),
            network: "bitcoin".into(),
        })
    }

    fn key(slot: usize, k: &str) -> WalletMsg {
        WalletMsg::Key(KeySubmission {
            proposal: "p1".into(),
            slot,
            key: k.into(),
            device: None,
        })
    }

    #[test]
    fn full_setup() {
        let k = keys();
        let mut msgs = vec![
            msg(0, A, proposal()),
            msg(1, A, key(0, &k[0])),
            // B tries to fill C's slot: ignored.
            msg(2, B, key(2, &k[1])),
            msg(3, B, key(1, &k[1])),
            msg(4, C, key(2, &k[2])),
        ];
        let s = reduce(&msgs, "p1", Network::Bitcoin).unwrap();
        assert!(s.all_keys_in() && !s.finalized);
        assert_eq!(s.problems.len(), 1);
        let d = s.descriptor.clone().unwrap();
        let h = s.hash.clone().unwrap();
        let addr = s.first_address.clone().unwrap();
        assert!(d.starts_with("wsh(sortedmulti(2,"));

        // B can't finalize; a wrong descriptor is refused; the right one counts.
        let fin = |desc: &str, hash: &str| {
            WalletMsg::Finalize(Finalize {
                proposal: "p1".into(),
                descriptor: desc.into(),
                hash: hash.into(),
            })
        };
        msgs.push(msg(5, B, fin(&d, &h)));
        msgs.push(msg(6, A, fin("wsh(sortedmulti(1,xpub))", &h)));
        msgs.push(msg(7, A, fin(&d, &h)));
        // A key after finalize is ignored.
        msgs.push(msg(8, C, key(2, &k[0])));
        let s = reduce(&msgs, "p1", Network::Bitcoin).unwrap();
        assert!(s.finalized && !s.ready);
        assert_eq!(s.slots[2].key.as_deref(), Some(k[2].as_str()));

        let conf = |a: &str| {
            WalletMsg::Confirm(Confirm {
                proposal: "p1".into(),
                hash: h.clone(),
                address: a.into(),
            })
        };
        msgs.push(msg(9, A, conf(&addr)));
        msgs.push(msg(10, B, conf("bc1qwrong")));
        msgs.push(msg(11, B, conf(&addr)));
        // A duplicate (same id) changes nothing.
        msgs.push(msg(11, C, conf(&addr)));
        let s = reduce(&msgs, "p1", Network::Bitcoin).unwrap();
        assert!(!s.ready);
        assert!(s.slots[0].confirmed && s.slots[1].confirmed && !s.slots[2].confirmed);
        msgs.push(msg(12, C, conf(&addr)));
        assert!(reduce(&msgs, "p1", Network::Bitcoin).unwrap().ready);
        assert_eq!(short_code(&h).len(), 19);
    }

    #[test]
    fn duplicate_key_and_wrong_network() {
        let k = keys();
        let msgs = vec![
            msg(0, A, proposal()),
            msg(1, A, key(0, &k[0])),
            msg(2, B, key(1, &k[0])),
        ];
        let s = reduce(&msgs, "p1", Network::Bitcoin).unwrap();
        assert!(s.slots[1].key.is_none());
        assert!(reduce(&msgs, "p1", Network::Testnet).is_none());
        assert!(reduce(&msgs, "other", Network::Bitcoin).is_none());
    }
}
