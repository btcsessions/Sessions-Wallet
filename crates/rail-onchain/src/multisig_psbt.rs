//! A vault spend's PSBT: which keys have signed, where the money goes, and
//! adding a cosigner's signatures safely.
//!
//! Keys are told apart by their xpubs (deriving each input's child key), not
//! by fingerprints, which some files leave out (`00000000`). Signatures from
//! another PSBT are only taken after they verify against this transaction
//! (SIGHASH_ALL), and its finalized fields are never trusted as they are.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{ChildNumber, DerivationPath, Xpub};
use bdk_wallet::bitcoin::secp256k1::{Message, Secp256k1, Verification};
use bdk_wallet::bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bdk_wallet::bitcoin::{Address, Psbt, PublicKey, ScriptBuf};
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
use sessions_core::{Error, Network, Result};

use super::MultisigConfig;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

/// `(branch, index)` of an input or output, from the last two levels of any
/// of its key paths.
fn branch_index(paths: impl Iterator<Item = DerivationPath>) -> Option<(u32, u32)> {
    for p in paths {
        let v: Vec<ChildNumber> = p.into_iter().copied().collect();
        if v.len() >= 2 {
            if let (ChildNumber::Normal { index: b }, ChildNumber::Normal { index: i }) =
                (v[v.len() - 2], v[v.len() - 1])
            {
                return Some((b, i));
            }
        }
    }
    None
}

/// Each cosigner's public key at `branch/index`, in config order.
fn child_keys<C: Verification>(
    secp: &Secp256k1<C>,
    config: &MultisigConfig,
    branch: u32,
    index: u32,
) -> Result<Vec<PublicKey>> {
    config
        .cosigners
        .iter()
        .map(|c| {
            let x = Xpub::from_str(&c.xpub).map_err(err)?;
            let path = [
                ChildNumber::from_normal_idx(branch).map_err(err)?,
                ChildNumber::from_normal_idx(index).map_err(err)?,
            ];
            Ok(PublicKey::new(
                x.derive_pub(secp, &path).map_err(err)?.public_key,
            ))
        })
        .collect()
}

fn input_branch_index(psbt: &Psbt, i: usize) -> Option<(u32, u32)> {
    branch_index(
        psbt.inputs[i]
            .bip32_derivation
            .values()
            .map(|(_, p)| p.clone()),
    )
}

/// Per cosigner (config order): has it signed every input?
pub fn signers(config: &MultisigConfig, psbt: &Psbt) -> Result<Vec<bool>> {
    let secp = Secp256k1::verification_only();
    let mut out = vec![true; config.cosigners.len()];
    if psbt.inputs.is_empty() {
        return Ok(vec![false; config.cosigners.len()]);
    }
    for i in 0..psbt.inputs.len() {
        let Some((b, idx)) = input_branch_index(psbt, i) else {
            return Err(err("This transaction is missing its key paths"));
        };
        let keys = child_keys(&secp, config, b, idx)?;
        for (k, signed) in keys.iter().zip(out.iter_mut()) {
            *signed &= psbt.inputs[i].partial_sigs.contains_key(k);
        }
    }
    Ok(out)
}

/// The script of the vault's `branch` (0 receive, 1 change) at `index`.
fn vault_script(config: &MultisigConfig, branch: u32, index: u32) -> Result<ScriptBuf> {
    let pair = config.descriptors()?;
    let d = if branch == 0 {
        &pair.external
    } else {
        &pair.internal
    };
    let d = Descriptor::<DescriptorPublicKey>::from_str(d).map_err(err)?;
    Ok(d.at_derivation_index(index).map_err(err)?.script_pubkey())
}

/// Outputs that aren't the vault's own (its change or receive addresses):
/// `(address, sats)`. An output only counts as the vault's when its script
/// really is one the vault derives, whatever the PSBT claims.
pub fn destinations(
    config: &MultisigConfig,
    psbt: &Psbt,
    network: Network,
) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for (txo, o) in psbt.unsigned_tx.output.iter().zip(&psbt.outputs) {
        let ours = branch_index(o.bip32_derivation.values().map(|(_, p)| p.clone()))
            .filter(|(b, _)| *b <= 1)
            .and_then(|(b, i)| vault_script(config, b, i).ok())
            .is_some_and(|s| s == txo.script_pubkey);
        if !ours {
            let address = Address::from_script(&txo.script_pubkey, network.to_bitcoin())
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "an unusual output".into());
            out.push((address, txo.value.to_sat()));
        }
    }
    Ok(out)
}

/// The sighash a signature for input `i` must commit to.
fn digest(psbt: &Psbt, i: usize) -> Result<Message> {
    let input = &psbt.inputs[i];
    let prev = psbt.spend_utxo(i).map_err(err)?;
    let mut cache = SighashCache::new(&psbt.unsigned_tx);
    let hash: [u8; 32] = if let Some(ws) = &input.witness_script {
        *cache
            .p2wsh_signature_hash(i, ws, prev.value, EcdsaSighashType::All)
            .map_err(err)?
            .as_ref()
    } else if let Some(rs) = &input.redeem_script {
        *cache
            .legacy_signature_hash(i, rs, EcdsaSighashType::All.to_u32())
            .map_err(err)?
            .as_ref()
    } else {
        return Err(err("An input is missing its script"));
    };
    Ok(Message::from_digest(hash))
}

/// Add `theirs`' valid signatures to `ours` (same transaction). Returns how
/// many were new. Signatures already here are never replaced; signatures in
/// a finalized input are read out of its witness and checked like the rest.
pub fn combine_verified(config: &MultisigConfig, ours: &mut Psbt, theirs: &Psbt) -> Result<usize> {
    if ours.unsigned_tx.compute_txid() != theirs.unsigned_tx.compute_txid()
        || ours.inputs.len() != theirs.inputs.len()
    {
        return Err(err("That's a different transaction. Sign this one"));
    }
    let secp = Secp256k1::verification_only();
    let mut added = 0;
    for i in 0..ours.inputs.len() {
        let Some((b, idx)) = input_branch_index(ours, i) else {
            return Err(err("This transaction is missing its key paths"));
        };
        let keys = child_keys(&secp, config, b, idx)?;
        let msg = digest(ours, i)?;
        // Candidate signatures: theirs, plus any in a finalized witness.
        let mut candidates: Vec<Vec<u8>> = theirs.inputs[i]
            .partial_sigs
            .values()
            .map(|s| s.to_vec())
            .collect();
        if let Some(w) = &theirs.inputs[i].final_script_witness {
            let items: Vec<&[u8]> = w.iter().collect();
            if items.len() > 2 {
                candidates.extend(
                    items[1..items.len() - 1]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_vec()),
                );
            }
        }
        for raw in candidates {
            let Ok(sig) = bdk_wallet::bitcoin::ecdsa::Signature::from_slice(&raw) else {
                continue;
            };
            if sig.sighash_type != EcdsaSighashType::All {
                continue;
            }
            let mut normalized = sig.signature;
            normalized.normalize_s();
            for k in &keys {
                if ours.inputs[i].partial_sigs.contains_key(k) {
                    continue;
                }
                if secp.verify_ecdsa(&msg, &normalized, &k.inner).is_ok() {
                    ours.inputs[i].partial_sigs.insert(
                        *k,
                        bdk_wallet::bitcoin::ecdsa::Signature {
                            signature: normalized,
                            sighash_type: EcdsaSighashType::All,
                        },
                    );
                    added += 1;
                    break;
                }
            }
        }
    }
    Ok(added)
}
