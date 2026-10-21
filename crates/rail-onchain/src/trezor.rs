//! Trezor signing.
//!
//! Trezor firmware doesn't sign PSBTs. Trezor Connect's `signTransaction`
//! wants the inputs and outputs spelled out with derivation paths, plus the
//! full previous transactions for SegWit v0 inputs. This module turns our
//! unsigned PSBT into those parameters, and puts the signed transaction that
//! comes back into the PSBT so the normal finalize-and-broadcast path runs.

use std::str::FromStr;

use bdk_wallet::bitcoin::bip32::{DerivationPath, Fingerprint};
use bdk_wallet::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bdk_wallet::bitcoin::{Address, Psbt, Script, Transaction};
use serde_json::{json, Value};
use sessions_core::{Error, Network, Result};

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

/// Raw hex of a finalized PSBT's transaction (tests and debugging).
#[allow(dead_code)]
pub(crate) fn extract_hex(psbt: Psbt) -> Result<String> {
    Ok(serialize_hex(&psbt.extract_tx().map_err(err)?))
}
