//! Trezor signing.
//!
//! Trezor firmware doesn't sign PSBTs. Trezor Connect's `signTransaction`
//! wants the inputs and outputs spelled out with derivation paths, plus the
//! full previous transactions for SegWit v0 inputs. This module turns our
//! unsigned PSBT into those parameters, and puts the signed transaction that
//! comes back into the PSBT so the normal finalize-and-broadcast path runs.
//! Vault (multisig) PSBTs take [`sign_request_multisig`] and
//! [`merge_multisig_signatures`] instead: the Trezor adds one signature of
//! several, so nothing is finalized.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{DerivationPath, Fingerprint, Xpub};
use bdk_wallet::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bdk_wallet::bitcoin::{Address, Psbt, Script, Transaction};
use bdk_wallet::miniscript::descriptor::{Descriptor, DescriptorPublicKey};
use serde_json::{json, Value};
use sessions_core::{Error, Network, Result};

use crate::multisig::{MultisigConfig, Policy, ScriptType};

fn err(e: impl std::fmt::Display) -> Error {
    Error::rail("on-chain", e)
}

fn path_numbers(path: &DerivationPath) -> Vec<u32> {
    path.into_iter().map(|c| u32::from(*c)).collect()
}

/// Input script type, by the kind of coin being spent.
fn spend_type(spk: &Script) -> Result<&'static str> {
    Ok(if spk.is_p2wpkh() {
        "SPENDWITNESS"
    } else if spk.is_p2tr() {
        "SPENDTAPROOT"
    } else if spk.is_p2sh() {
        "SPENDP2SHWITNESS"
    } else if spk.is_p2pkh() {
        "SPENDADDRESS"
    } else {
        return Err(Error::Unsupported(
            "sign this kind of coin on a Trezor yet".into(),
        ));
    })
}

/// The same for change going back to the device.
fn change_type(spend: &str) -> &'static str {
    match spend {
        "SPENDWITNESS" => "PAYTOWITNESS",
        "SPENDTAPROOT" => "PAYTOTAPROOT",
        "SPENDP2SHWITNESS" => "PAYTOP2SHWITNESS",
        _ => "PAYTOADDRESS",
    }
}

/// The one path a single-sig input or output was derived on, if any.
fn our_path(
    bip32: &std::collections::BTreeMap<
        bdk_wallet::bitcoin::secp256k1::PublicKey,
        (Fingerprint, DerivationPath),
    >,
    tap: &std::collections::BTreeMap<
        bdk_wallet::bitcoin::XOnlyPublicKey,
        (
            Vec<bdk_wallet::bitcoin::TapLeafHash>,
            (Fingerprint, DerivationPath),
        ),
    >,
) -> Option<DerivationPath> {
    bip32
        .values()
        .map(|(_, p)| p.clone())
        .chain(tap.values().map(|(_, (_, p))| p.clone()))
        .next()
}

fn ref_tx(tx: &Transaction) -> Value {
    json!({
        "hash": tx.compute_txid().to_string(),
        "version": tx.version.0,
        "lock_time": tx.lock_time.to_consensus_u32(),
        "inputs": tx.input.iter().map(|i| json!({
            "prev_hash": i.previous_output.txid.to_string(),
            "prev_index": i.previous_output.vout,
            "script_sig": i.script_sig.to_hex_string(),
            "sequence": i.sequence.0,
        })).collect::<Vec<_>>(),
        "bin_outputs": tx.output.iter().map(|o| json!({
            "amount": o.value.to_sat().to_string(),
            "script_pubkey": o.script_pubkey.to_hex_string(),
        })).collect::<Vec<_>>(),
    })
}

/// Trezor Connect `signTransaction` parameters for an unsigned PSBT.
pub fn sign_request(psbt_b64: &str, network: Network) -> Result<Value> {
    let psbt = Psbt::from_str(psbt_b64.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))?;
    let tx = &psbt.unsigned_tx;

    let mut inputs = Vec::new();
    let mut ref_txs = Vec::new();
    let mut all_taproot = true;
    let mut first_type = "SPENDWITNESS";
    for (i, (txin, input)) in tx.input.iter().zip(&psbt.inputs).enumerate() {
        let prev = psbt.spend_utxo(i).map_err(err)?;
        let script_type = spend_type(&prev.script_pubkey)?;
        if i == 0 {
            first_type = script_type;
        }
        all_taproot &= script_type == "SPENDTAPROOT";
        let path = our_path(&input.bip32_derivation, &input.tap_key_origins).ok_or_else(|| {
            Error::Unsupported("sign an input that isn't from this hardware wallet".into())
        })?;
        inputs.push(json!({
            "address_n": path_numbers(&path),
            "prev_hash": txin.previous_output.txid.to_string(),
            "prev_index": txin.previous_output.vout,
            "amount": prev.value.to_sat().to_string(),
            "sequence": txin.sequence.0,
            "script_type": script_type,
        }));
        if let Some(full) = &input.non_witness_utxo {
            if !ref_txs
                .iter()
                .any(|r: &Value| r["hash"] == json!(full.compute_txid().to_string()))
            {
                ref_txs.push(ref_tx(full));
            }
        }
    }

    let net = network.to_bitcoin();
    let mut outputs = Vec::new();
    for (txout, output) in tx.output.iter().zip(&psbt.outputs) {
        let amount = txout.value.to_sat().to_string();
        if txout.script_pubkey.is_op_return() {
            let data = txout.script_pubkey.as_bytes().get(2..).unwrap_or_default();
            outputs.push(json!({
                "op_return_data": data.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "amount": "0",
                "script_type": "PAYTOOPRETURN",
            }));
        } else if let Some(path) = our_path(&output.bip32_derivation, &output.tap_key_origins) {
            let kind = spend_type(&txout.script_pubkey).unwrap_or(first_type);
            outputs.push(json!({
                "address_n": path_numbers(&path),
                "amount": amount,
                "script_type": change_type(kind),
            }));
        } else {
            let address = Address::from_script(&txout.script_pubkey, net).map_err(err)?;
            outputs.push(json!({
                "address": address.to_string(),
                "amount": amount,
                "script_type": "PAYTOADDRESS",
            }));
        }
    }

    let mut params = json!({
        "coin": if network == Network::Bitcoin { "btc" } else { "test" },
        "inputs": inputs,
        "outputs": outputs,
        "version": tx.version.0,
        "locktime": tx.lock_time.to_consensus_u32(),
        "push": false,
    });
    // SegWit v0 needs the previous transactions. Passing them keeps Trezor
    // Suite from looking them up on its own servers.
    if !all_taproot && !ref_txs.is_empty() {
        params["refTxs"] = Value::Array(ref_txs);
    }
    Ok(params)
}

/// Put a fully signed transaction from the device back into the PSBT, as
/// finalized inputs, so it goes through the usual broadcast path.
pub fn merge_signed_tx(psbt_b64: &str, raw_tx_hex: &str) -> Result<String> {
    let mut psbt =
        Psbt::from_str(psbt_b64.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))?;
    let signed: Transaction = deserialize_hex(raw_tx_hex.trim())
        .map_err(|e| err(format!("unreadable signed transaction: {e}")))?;
    if signed.compute_txid() != psbt.unsigned_tx.compute_txid() {
        return Err(err(
            "the device signed a different transaction. Nothing was sent",
        ));
    }
    for (input, txin) in psbt.inputs.iter_mut().zip(&signed.input) {
        if txin.witness.is_empty() && txin.script_sig.is_empty() {
            return Err(err("the device left an input unsigned"));
        }
        if !txin.witness.is_empty() {
            input.final_script_witness = Some(txin.witness.clone());
        }
        if !txin.script_sig.is_empty() {
            input.final_script_sig = Some(txin.script_sig.clone());
        }
    }
    Ok(psbt.to_string())
}

// --- multisig ---------------------------------------------------------------
//
// For a vault, every input and every change output names the whole quorum:
// `multisig: { nodes, address_n: [change, index], m, signatures,
// pubkeys_order }` with the cosigners' account xpubs as `HDNodeType`s. One
// list of nodes for the whole transaction (rather than per-key paths, which
// firmware 2.8.7 refuses for change and addresses) lets the Trezor recognise
// change. `sortedmulti` is `pubkeys_order: LEXICOGRAPHIC` (Trezor core 2.8.7+,
// Trezor One 1.13.0+).
//
// The Trezor signs only with its own key and returns one DER signature per
// input; [`merge_multisig_signatures`] checks each against the input's keys
// and adds it to `partial_sigs` without finalizing, so the other cosigners
// can still sign.

/// `MultisigPubkeysOrder`.
const ORDER_PRESERVED: u32 = 0;
const ORDER_LEXICOGRAPHIC: u32 = 1;

/// Trezor's `HDNodeType` for an xpub (`fingerprint` is the parent's).
fn hd_node(xpub: &Xpub) -> Value {
    json!({
        "depth": xpub.depth,
        "fingerprint": u32::from_be_bytes(xpub.parent_fingerprint.to_bytes()),
        "child_num": u32::from(xpub.child_number),
        "chain_code": hex(xpub.chain_code.as_bytes()),
        "public_key": hex(&xpub.public_key.serialize()),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Input and change script types for a vault.
fn multisig_types(script: ScriptType) -> (&'static str, &'static str) {
    match script {
        ScriptType::P2wsh => ("SPENDWITNESS", "PAYTOWITNESS"),
        ScriptType::P2shP2wsh => ("SPENDP2SHWITNESS", "PAYTOP2SHWITNESS"),
        ScriptType::P2sh => ("SPENDMULTISIG", "PAYTOMULTISIG"),
    }
}

/// A vault as the Trezor sees it.
struct Vault {
    /// The Trezor's key: index in the key list, and its account path.
    ours: (usize, DerivationPath),
    nodes: Vec<Value>,
    m: u8,
    order: u32,
    script: ScriptType,
    /// Receive and change descriptors, to recognise the vault's scripts.
    branches: [Descriptor<DescriptorPublicKey>; 2],
}

impl Vault {
    fn new(config: &MultisigConfig, fingerprint: &str) -> Result<Self> {
        let fp = Fingerprint::from_str(fingerprint.trim())
            .map_err(|_| Error::Invalid("The Trezor returned an unreadable fingerprint".into()))?;
        let ours = config
            .cosigners
            .iter()
            .position(|c| c.fingerprint.eq_ignore_ascii_case(&fp.to_string()))
            .ok_or_else(|| Error::Invalid("This Trezor isn't one of this vault's keys".into()))?;
        let path = DerivationPath::from_str(&config.cosigners[ours].path).map_err(err)?;
        let nodes = config
            .cosigners
            .iter()
            .map(|c| Xpub::from_str(&c.xpub).map(|x| hd_node(&x)).map_err(err))
            .collect::<Result<Vec<_>>>()?;
        let Policy::Multi { threshold, sorted } = config.policy;
        let pair = config.descriptors()?;
        let parse = |s: &str| {
            Descriptor::<DescriptorPublicKey>::from_str(s)
                .map_err(|e| Error::Crypto(format!("bad descriptor: {e}")))
        };
        Ok(Vault {
            ours: (ours, path),
            nodes,
            m: threshold,
            order: if sorted {
                ORDER_LEXICOGRAPHIC
            } else {
                ORDER_PRESERVED
            },
            script: config.script,
            branches: [parse(&pair.external)?, parse(&pair.internal)?],
        })
    }

    /// The `multisig` field for the script at `[change, index]`.
    fn field(&self, change: u32, index: u32) -> Value {
        json!({
            "pubkeys": [],
            "nodes": self.nodes,
            "address_n": [change, index],
            "signatures": vec![""; self.nodes.len()],
            "m": self.m,
            "pubkeys_order": self.order,
        })
    }

    /// Our full keypath for `[change, index]`.
    fn address_n(&self, change: u32, index: u32) -> Vec<u32> {
        let mut path = path_numbers(&self.ours.1);
        path.extend([change, index]);
        path
    }

    /// `[change, index]` of a vault script, if `spk` is one: the key origin
    /// of our key in `bip32` gives the position, and the script must be the
    /// vault's at that position.
    fn position(
        &self,
        spk: &Script,
        bip32: &std::collections::BTreeMap<
            bdk_wallet::bitcoin::secp256k1::PublicKey,
            (Fingerprint, DerivationPath),
        >,
        fingerprint: Fingerprint,
    ) -> Option<(u32, u32)> {
        let account = path_numbers(&self.ours.1);
        bip32.values().find_map(|(fp, path)| {
            let path = path_numbers(path);
            if *fp != fingerprint
                || path.len() != account.len() + 2
                || path[..account.len()] != account[..]
            {
                return None;
            }
            let (change, index) = (path[account.len()], path[account.len() + 1]);
            if change > 1 || index >= 0x8000_0000 {
                return None;
            }
            let script = self.branches[change as usize]
                .at_derivation_index(index)
                .ok()?
                .script_pubkey();
            (script == *spk).then_some((change, index))
        })
    }
}

/// Trezor Connect `signTransaction` parameters for a vault PSBT, for the
/// Trezor whose root fingerprint is `fingerprint` (hex). Every input must be
/// the vault's.
pub fn sign_request_multisig(
    psbt_b64: &str,
    config: &MultisigConfig,
    network: Network,
    fingerprint: &str,
) -> Result<Value> {
    let psbt = Psbt::from_str(psbt_b64.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))?;
    let vault = Vault::new(config, fingerprint)?;
    let fp = Fingerprint::from_str(fingerprint.trim()).map_err(err)?;
    let (spend, pay) = multisig_types(vault.script);
    let tx = &psbt.unsigned_tx;

    let mut inputs = Vec::new();
    let mut ref_txs: Vec<Value> = Vec::new();
    for (i, (txin, input)) in tx.input.iter().zip(&psbt.inputs).enumerate() {
        let prev = psbt.spend_utxo(i).map_err(err)?;
        let (change, index) = vault
            .position(&prev.script_pubkey, &input.bip32_derivation, fp)
            .ok_or_else(|| {
                Error::Unsupported("sign an input that isn't from this vault on a Trezor".into())
            })?;
        inputs.push(json!({
            "address_n": vault.address_n(change, index),
            "prev_hash": txin.previous_output.txid.to_string(),
            "prev_index": txin.previous_output.vout,
            "amount": prev.value.to_sat().to_string(),
            "sequence": txin.sequence.0,
            "script_type": spend,
            "multisig": vault.field(change, index),
        }));
        if let Some(full) = &input.non_witness_utxo {
            if !ref_txs
                .iter()
                .any(|r| r["hash"] == json!(full.compute_txid().to_string()))
            {
                ref_txs.push(ref_tx(full));
            }
        }
    }

    let net = network.to_bitcoin();
    let mut outputs = Vec::new();
    for (txout, output) in tx.output.iter().zip(&psbt.outputs) {
        let amount = txout.value.to_sat().to_string();
        if txout.script_pubkey.is_op_return() {
            let data = txout.script_pubkey.as_bytes().get(2..).unwrap_or_default();
            outputs.push(json!({
                "op_return_data": hex(data),
                "amount": "0",
                "script_type": "PAYTOOPRETURN",
            }));
        } else if let Some((change, index)) =
            vault.position(&txout.script_pubkey, &output.bip32_derivation, fp)
        {
            outputs.push(json!({
                "address_n": vault.address_n(change, index),
                "amount": amount,
                "script_type": pay,
                "multisig": vault.field(change, index),
            }));
        } else {
            let address = Address::from_script(&txout.script_pubkey, net).map_err(err)?;
            outputs.push(json!({
                "address": address.to_string(),
                "amount": amount,
                "script_type": "PAYTOADDRESS",
            }));
        }
    }

    let mut params = json!({
        "coin": if network == Network::Bitcoin { "btc" } else { "test" },
        "inputs": inputs,
        "outputs": outputs,
        "version": tx.version.0,
        "locktime": tx.lock_time.to_consensus_u32(),
        "push": false,
    });
    if !ref_txs.is_empty() {
        params["refTxs"] = Value::Array(ref_txs);
    }
    Ok(params)
}

/// Trezor Connect `getAddress` parameters to show vault receive address
/// `index` on the Trezor (root fingerprint `fingerprint`), and the address
/// this wallet expects it to show.
pub fn multisig_address_request(
    config: &MultisigConfig,
    network: Network,
    fingerprint: &str,
    index: u32,
) -> Result<(Value, String)> {
    if index >= 0x8000_0000 {
        return Err(Error::Invalid("That address number is too large".into()));
    }
    let vault = Vault::new(config, fingerprint)?;
    let (spend, _) = multisig_types(vault.script);
    let address = vault.branches[0]
        .at_derivation_index(index)
        .map_err(err)?
        .address(network.to_bitcoin())
        .map_err(err)?
        .to_string();
    let params = json!({
        "path": vault.address_n(0, index),
        "coin": if network == Network::Bitcoin { "btc" } else { "test" },
        "scriptType": spend,
        "multisig": vault.field(0, index),
        "showOnTrezor": true,
    });
    Ok((params, address))
}

/// Add the Trezor's signatures (Connect's `signatures`: one DER hex per
/// input, missing or empty where it didn't sign) to the PSBT's
/// `partial_sigs`, under the key each one verifies against. Nothing is
/// finalized: other cosigners may still need to sign. `signed_tx`, Connect's
/// `serializedTx`, if given, must be the same transaction.
pub fn merge_multisig_signatures(
    psbt_b64: &str,
    signatures: &[Option<String>],
    signed_tx: Option<&str>,
) -> Result<String> {
    use bdk_wallet::bitcoin::secp256k1::{ecdsa, Message, Secp256k1};
    use bdk_wallet::bitcoin::sighash::{EcdsaSighashType, SighashCache};

    let mut psbt =
        Psbt::from_str(psbt_b64.trim()).map_err(|e| err(format!("invalid PSBT: {e}")))?;
    let different = || err("the Trezor signed a different transaction. Nothing was changed");
    if let Some(raw) = signed_tx.map(str::trim).filter(|s| !s.is_empty()) {
        let signed: Transaction = deserialize_hex(raw)
            .map_err(|e| err(format!("unreadable signed transaction: {e}")))?;
        if signed.compute_txid() != psbt.unsigned_tx.compute_txid() {
            return Err(different());
        }
    }
    if signatures.len() > psbt.inputs.len() {
        return Err(different());
    }
    let secp = Secp256k1::verification_only();
    let mut added = 0;
    for (i, sig) in signatures.iter().enumerate() {
        let Some(sig) = sig.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let bytes = (0..sig.len())
            .step_by(2)
            .map(|j| sig.get(j..j + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(|| err("the Trezor returned an unreadable signature"))?;
        // DER, as Trezor returns it; tolerate a trailing SIGHASH_ALL byte.
        let mut signature = ecdsa::Signature::from_der(&bytes)
            .or_else(|_| {
                bdk_wallet::bitcoin::ecdsa::Signature::from_slice(&bytes).map(|s| s.signature)
            })
            .map_err(|_| err("the Trezor returned an unreadable signature"))?;
        signature.normalize_s();

        let input = &psbt.inputs[i];
        let prev = psbt.spend_utxo(i).map_err(err)?.clone();
        let mut cache = SighashCache::new(&psbt.unsigned_tx);
        let digest: [u8; 32] = if let Some(ws) = &input.witness_script {
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
            return Err(err("an input is missing its script"));
        };
        let msg = Message::from_digest(digest);
        let key = input
            .bip32_derivation
            .keys()
            .find(|pk| secp.verify_ecdsa(&msg, &signature, pk).is_ok())
            .copied()
            .ok_or_else(|| {
                err("the Trezor's signature doesn't match this vault's keys. Nothing was changed")
            })?;
        psbt.inputs[i].partial_sigs.insert(
            bdk_wallet::bitcoin::PublicKey::new(key),
            bdk_wallet::bitcoin::ecdsa::Signature {
                signature,
                sighash_type: EcdsaSighashType::All,
            },
        );
        added += 1;
    }
    if added == 0 {
        return Err(err("the Trezor didn't sign anything"));
    }
    Ok(psbt.to_string())
}

/// Raw hex of a finalized PSBT's transaction (tests and debugging).
#[allow(dead_code)]
pub(crate) fn extract_hex(psbt: Psbt) -> Result<String> {
    Ok(serialize_hex(&psbt.extract_tx().map_err(err)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitbox::vault_fixture as fx;
    use crate::descriptors;
    use bdk_wallet::bitcoin::secp256k1::{Message, Secp256k1};
    use bdk_wallet::bitcoin::sighash::{EcdsaSighashType, SighashCache};
    use bdk_wallet::miniscript::psbt::PsbtExt;

    const H: u32 = 0x8000_0000;

    fn fingerprint_of(phrase: &str) -> String {
        fx::master(phrase).fingerprint(&Secp256k1::new()).to_string()
    }

    /// What a device holding `phrase` returns for input 0: a DER signature
    /// with the key at `path`.
    fn device_signature(psbt: &Psbt, phrase: &str, path: &str) -> String {
        let secp = Secp256k1::new();
        let key = fx::master(phrase)
            .derive_priv(&secp, &DerivationPath::from_str(path).unwrap())
            .unwrap();
        let prev = psbt.spend_utxo(0).unwrap();
        let ws = psbt.inputs[0].witness_script.as_ref().unwrap();
        let digest = SighashCache::new(&psbt.unsigned_tx)
            .p2wsh_signature_hash(0, ws, prev.value, EcdsaSighashType::All)
            .unwrap();
        let sig = secp.sign_ecdsa(&Message::from_digest(*digest.as_ref()), &key.private_key);
        hex(&sig.serialize_der())
    }

    #[test]
    fn multisig_request_names_the_whole_quorum() {
        let (config, pair) = fx::vault();
        let psbt = fx::psbt(&pair).to_string();
        let params = sign_request_multisig(&psbt, &config, Network::Bitcoin, "73c5da0a").unwrap();
        assert_eq!(params["coin"], "btc");
        assert_eq!(params["refTxs"].as_array().unwrap().len(), 1);

        let input = &params["inputs"][0];
        assert_eq!(input["address_n"], json!([48 | H, H, H, 2 | H, 0, 0]));
        assert_eq!(input["script_type"], "SPENDWITNESS");
        assert_eq!(input["amount"], "100000");
        let ms = &input["multisig"];
        assert_eq!(ms["m"], 2);
        assert_eq!(ms["address_n"], json!([0, 0]));
        assert_eq!(ms["pubkeys_order"], ORDER_LEXICOGRAPHIC);
        assert_eq!(ms["pubkeys"], json!([]));
        assert_eq!(ms["signatures"], json!(["", "", ""]));
        let nodes = ms["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        for (node, c) in nodes.iter().zip(&config.cosigners) {
            let x = Xpub::from_str(&c.xpub).unwrap();
            assert_eq!(node["public_key"], hex(&x.public_key.serialize()));
            assert_eq!(node["chain_code"], hex(x.chain_code.as_bytes()));
            assert_eq!(node["depth"], 4);
            assert_eq!(node["child_num"], 2 | H);
            assert_eq!(
                node["fingerprint"],
                u32::from_be_bytes(x.parent_fingerprint.to_bytes())
            );
        }

        assert_eq!(
            params["outputs"][0],
            json!({ "address": fx::PAY_TO, "amount": "60000", "script_type": "PAYTOADDRESS" })
        );
        let change = &params["outputs"][1];
        assert_eq!(change["address_n"], json!([48 | H, H, H, 2 | H, 1, 3]));
        assert_eq!(change["script_type"], "PAYTOWITNESS");
        assert_eq!(change["amount"], "39000");
        assert_eq!(change["multisig"]["address_n"], json!([1, 3]));
        // The same nodes everywhere: that's how the Trezor knows it's change.
        assert_eq!(change["multisig"]["nodes"], ms["nodes"]);

        // Each cosigner signs with its own key.
        let legal = fingerprint_of(fx::LEGAL);
        let theirs = sign_request_multisig(&psbt, &config, Network::Bitcoin, &legal).unwrap();
        assert_eq!(theirs["inputs"][0]["multisig"], *ms);

        let e = sign_request_multisig(&psbt, &config, Network::Bitcoin, "01020304").unwrap_err();
        assert_eq!(e.to_string(), "This Trezor isn't one of this vault's keys");
        // An input from somewhere else is refused rather than half-described.
        let mut foreign = fx::psbt(&pair);
        foreign.inputs[0].bip32_derivation.clear();
        assert!(
            sign_request_multisig(&foreign.to_string(), &config, Network::Bitcoin, "73c5da0a")
                .is_err()
        );
    }

    #[test]
    fn nested_and_unsorted_vaults_use_their_script_types() {
        let (mut config, _) = fx::vault();
        config.script = ScriptType::P2shP2wsh;
        config.policy = Policy::Multi {
            threshold: 2,
            sorted: false,
        };
        let pair = config.descriptors().unwrap();
        let psbt = fx::psbt(&pair).to_string();
        let params = sign_request_multisig(&psbt, &config, Network::Bitcoin, "73c5da0a").unwrap();
        assert_eq!(params["inputs"][0]["script_type"], "SPENDP2SHWITNESS");
        assert_eq!(
            params["inputs"][0]["multisig"]["pubkeys_order"],
            ORDER_PRESERVED
        );
        assert_eq!(params["outputs"][1]["script_type"], "PAYTOP2SHWITNESS");
    }

    #[test]
    fn address_request_matches_the_wallet() {
        let (config, pair) = fx::vault();
        let (params, address) =
            multisig_address_request(&config, Network::Bitcoin, "73c5da0a", 5).unwrap();
        assert_eq!(
            address,
            descriptors::address_at(&pair, Network::Bitcoin, 5).unwrap()
        );
        assert!(address.starts_with("bc1q") && address.len() == 62);
        assert_eq!(params["path"], json!([48 | H, H, H, 2 | H, 0, 5]));
        assert_eq!(params["scriptType"], "SPENDWITNESS");
        assert_eq!(params["showOnTrezor"], true);
        assert_eq!(params["multisig"]["address_n"], json!([0, 5]));
        assert!(multisig_address_request(&config, Network::Bitcoin, "01020304", 0).is_err());
    }

    #[test]
    fn trezor_signatures_are_kept_partial_until_enough_keys_signed() {
        let secp = Secp256k1::new();
        let (_, pair) = fx::vault();
        let psbt = fx::psbt(&pair);
        let b64 = psbt.to_string();
        let ours = Fingerprint::from_str("73c5da0a").unwrap();

        let sig = device_signature(&psbt, fx::ABANDON, "m/48'/0'/0'/2'/0/0");
        let one = merge_multisig_signatures(
            &b64,
            &[Some(sig)],
            Some(&serialize_hex(&psbt.unsigned_tx)),
        )
        .unwrap();
        let one = Psbt::from_str(&one).unwrap();
        assert_eq!(one.inputs[0].partial_sigs.len(), 1);
        assert!(crate::bitbox::signed_own_inputs(&one, ours));
        assert!(one.inputs[0].final_script_witness.is_none());
        assert!(one.clone().finalize_mut(&secp).is_err());

        // A second key (another cosigner's Trezor) completes the quorum.
        let sig = device_signature(&psbt, fx::LEGAL, "m/48'/0'/0'/2'/0/0");
        let two = merge_multisig_signatures(&one.to_string(), &[Some(sig)], None).unwrap();
        let mut two = Psbt::from_str(&two).unwrap();
        assert_eq!(two.inputs[0].partial_sigs.len(), 2);
        two.finalize_mut(&secp).unwrap();
        two.extract(&secp).unwrap();

        // A key outside the vault, nothing signed, another transaction: refused.
        let stranger = device_signature(&psbt, fx::ABANDON, "m/84'/0'/0'/0/0");
        assert!(merge_multisig_signatures(&b64, &[Some(stranger)], None).is_err());
        assert!(merge_multisig_signatures(&b64, &[None], None).is_err());
        assert!(merge_multisig_signatures(&b64, &[Some(String::new())], None).is_err());
        let mut other = psbt.unsigned_tx.clone();
        other.output[0].value = bdk_wallet::bitcoin::Amount::from_sat(1);
        let sig = device_signature(&psbt, fx::ABANDON, "m/48'/0'/0'/2'/0/0");
        assert!(merge_multisig_signatures(
            &b64,
            &[Some(sig.clone())],
            Some(&serialize_hex(&other))
        )
        .is_err());
        assert!(merge_multisig_signatures(&b64, &[Some(sig), None], None).is_err());
    }
}
