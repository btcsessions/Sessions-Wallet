//! A minimal in-process Cashu mint for offline tests.
//!
//! cdk 0.18's real `Mint` needs a mint database, and the only implementations
//! (cdk-sqlite, cdk-postgres) are off-limits here (cdk-sqlite's rusqlite clashes
//! with the Spark SDK). So this implements CDK's `MintConnector` directly:
//! real BDHKE blind signatures, proof verification and double-spend tracking,
//! with a fake Lightning side the test drives by hand (`pay_mint_quote`).
//! No DLEQ proofs, no fees on inputs, no NUT-20 signature check.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use cdk::dhke::{sign_message, verify_message};
use cdk::nuts::nut04::MintMethodOptions;
use cdk::nuts::{
    BatchCheckMintQuoteRequest, BatchMintRequest, BlindSignature, BlindedMessage,
    CheckStateRequest, CheckStateResponse, CurrencyUnit, Id, KeySet, KeySetInfo, Keys,
    KeysetResponse, MeltMethodSettings, MeltQuoteBolt11Response, MeltQuoteState, MeltRequest,
    MintInfo, MintMethodSettings, MintQuoteBolt11Response, MintQuoteState, MintRequest,
    MintResponse, NUT04Settings, NUT05Settings, Nuts, PaymentMethod, ProofState, Proofs, PublicKey,
    RestoreRequest, RestoreResponse, SecretKey, State, SwapRequest, SwapResponse,
};
use cdk::util::unix_time;
use cdk::wallet::{AuthWallet, LnurlPayInvoiceResponse, LnurlPayResponse, MintConnector};
use cdk::{
    Amount, Error, MeltQuoteCreateResponse, MeltQuoteRequest, MeltQuoteResponse, MintQuoteRequest,
    MintQuoteResponse,
};

/// Flat fee reserve the fake mint asks for on every melt.
pub const FEE_RESERVE: u64 = 4;
/// What the fake Lightning payment actually costs; the rest comes back as change.
pub const FEE_PAID: u64 = 1;

struct MintQ {
    amount: u64,
    request: String,
    state: MintQuoteState,
    pubkey: Option<PublicKey>,
}

struct MeltQ {
    amount: u64,
    request: String,
    state: MeltQuoteState,
}

#[derive(Default)]
struct State_ {
    mint_quotes: HashMap<String, MintQ>,
    melt_quotes: HashMap<String, MeltQ>,
    spent: HashSet<PublicKey>,
    counter: u64,
}

pub struct FakeMint {
    keys: BTreeMap<Amount, SecretKey>,
    keyset: KeySet,
    state: Mutex<State_>,
    /// Lightning invoices this mint "paid", for assertions.
    pub paid_invoices: Mutex<Vec<String>>,
}

impl std::fmt::Debug for FakeMint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeMint")
    }
}

impl FakeMint {
    /// `key_seed` makes different fake mints use different keys.
    pub fn new(key_seed: u8) -> Self {
        let mut keys = BTreeMap::new();
        let mut public = BTreeMap::new();
        for power in 0..32u8 {
            let amount = Amount::from(1u64 << power);
            let mut bytes = [key_seed; 32];
            bytes[31] = power + 1;
            let sk = SecretKey::from_slice(&bytes).expect("valid test key");
            public.insert(amount, sk.public_key());
            keys.insert(amount, sk);
        }
        let public = Keys::new(public);
        let keyset = KeySet {
            id: Id::v1_from_keys(&public),
            unit: CurrencyUnit::Sat,
            active: Some(true),
            keys: public,
            input_fee_ppk: 0,
            final_expiry: None,
        };
        Self {
            keys,
            keyset,
            state: Mutex::new(State_::default()),
            paid_invoices: Mutex::new(vec![]),
        }
    }

    /// Simulate the Lightning invoice behind a mint quote being paid.
    pub fn pay_mint_quote(&self, quote_id: &str) {
        let mut st = self.state.lock().unwrap();
        let q = st.mint_quotes.get_mut(quote_id).expect("known quote");
        assert_eq!(q.state, MintQuoteState::Unpaid);
        q.state = MintQuoteState::Paid;
    }

    fn next_id(st: &mut State_, prefix: &str) -> String {
        st.counter += 1;
        format!("{prefix}-{}", st.counter)
    }

    fn sign(&self, outputs: &[BlindedMessage]) -> Result<Vec<BlindSignature>, Error> {
        outputs
            .iter()
            .map(|o| {
                if o.keyset_id != self.keyset.id {
                    return Err(Error::UnknownKeySet);
                }
                let k = self.keys.get(&o.amount).ok_or(Error::AmountKey)?;
                Ok(BlindSignature {
                    amount: o.amount,
                    keyset_id: o.keyset_id,
                    c: sign_message(k, &o.blinded_secret)?,
                    dleq: None,
                })
            })
            .collect()
    }

    /// Verify signatures and that no input is spent; returns (total, Ys).
    fn verify_inputs(&self, st: &State_, inputs: &Proofs) -> Result<(u64, Vec<PublicKey>), Error> {
        let mut total = 0u64;
        let mut ys = Vec::with_capacity(inputs.len());
        for p in inputs {
            if p.keyset_id != self.keyset.id {
                return Err(Error::UnknownKeySet);
            }
            let k = self.keys.get(&p.amount).ok_or(Error::AmountKey)?;
            verify_message(k, p.c, p.secret.as_bytes())
                .map_err(|_| Error::Custom("proof signature invalid".into()))?;
            let y = p.y()?;
            if st.spent.contains(&y) || ys.contains(&y) {
                return Err(Error::TokenAlreadySpent);
            }
            ys.push(y);
            total += p.amount.to_u64();
        }
        Ok((total, ys))
    }

    fn mint_quote_response(id: &str, q: &MintQ) -> MintQuoteBolt11Response<String> {
        let (paid, issued) = match q.state {
            MintQuoteState::Unpaid => (0, 0),
            MintQuoteState::Paid => (q.amount, 0),
            MintQuoteState::Issued => (q.amount, q.amount),
        };
        MintQuoteBolt11Response {
            quote: id.to_string(),
            request: q.request.clone(),
            amount: Some(Amount::from(q.amount)),
            unit: Some(CurrencyUnit::Sat),
            method: PaymentMethod::BOLT11,
            amount_paid: Amount::from(paid),
            amount_issued: Amount::from(issued),
            updated_at: unix_time(),
            state: q.state,
            expiry: Some(unix_time() + 3600),
            pubkey: q.pubkey,
        }
    }

    fn melt_quote_response(
        id: &str,
        q: &MeltQ,
        change: Option<Vec<BlindSignature>>,
    ) -> MeltQuoteBolt11Response<String> {
        let paid = q.state == MeltQuoteState::Paid;
        MeltQuoteBolt11Response {
            quote: id.to_string(),
            amount: Amount::from(q.amount),
            fee_reserve: Amount::from(FEE_RESERVE),
            state: q.state,
            expiry: unix_time() + 3600,
            payment_preimage: paid.then(|| "00".repeat(32)),
            change,
            request: Some(q.request.clone()),
            unit: Some(CurrencyUnit::Sat),
            method: PaymentMethod::BOLT11,
        }
    }
}

/// A syntactically valid mainnet BOLT11 invoice (never payable).
pub fn fake_invoice(amount_sat: u64, tag: u64) -> String {
    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::secp256k1::{Secp256k1, SecretKey as SK};
    use cdk::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

    let node_key = SK::from_slice(&[0x42; 32]).unwrap();
    InvoiceBuilder::new(Currency::Bitcoin)
        .description("fake".into())
        .payment_hash(sha256::Hash::hash(&tag.to_be_bytes()))
        .payment_secret(PaymentSecret([7u8; 32]))
        .amount_milli_satoshis(amount_sat * 1000)
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .build_signed(|h| Secp256k1::new().sign_ecdsa_recoverable(h, &node_key))
        .unwrap()
        .to_string()
}

#[async_trait]
impl MintConnector for FakeMint {
    async fn fetch_lnurl_pay_request(&self, _url: &str) -> Result<LnurlPayResponse, Error> {
        Err(Error::UnsupportedPaymentMethod)
    }

    async fn fetch_lnurl_invoice(&self, _url: &str) -> Result<LnurlPayInvoiceResponse, Error> {
        Err(Error::UnsupportedPaymentMethod)
    }

    async fn get_mint_keys(&self) -> Result<Vec<KeySet>, Error> {
        Ok(vec![self.keyset.clone()])
    }

    async fn get_mint_keyset(&self, keyset_id: Id) -> Result<KeySet, Error> {
        if keyset_id == self.keyset.id {
            Ok(self.keyset.clone())
        } else {
            Err(Error::UnknownKeySet)
        }
    }

    async fn get_mint_keysets(&self) -> Result<KeysetResponse, Error> {
        Ok(KeysetResponse {
            keysets: vec![KeySetInfo {
                id: self.keyset.id,
                unit: CurrencyUnit::Sat,
                active: true,
                input_fee_ppk: 0,
                final_expiry: None,
            }],
        })
    }

    async fn post_mint_quote(
        &self,
        request: MintQuoteRequest,
    ) -> Result<MintQuoteResponse<String>, Error> {
        let MintQuoteRequest::Bolt11(req) = request else {
            return Err(Error::UnsupportedPaymentMethod);
        };
        let mut st = self.state.lock().unwrap();
        let id = Self::next_id(&mut st, "mintq");
        let counter = st.counter;
        let q = MintQ {
            amount: req.amount.to_u64(),
            request: fake_invoice(req.amount.to_u64(), 1_000_000 + counter),
            state: MintQuoteState::Unpaid,
            pubkey: req.pubkey,
        };
        let resp = Self::mint_quote_response(&id, &q);
        st.mint_quotes.insert(id, q);
        Ok(MintQuoteResponse::Bolt11(resp))
    }

    async fn post_mint(
        &self,
        _method: &PaymentMethod,
        request: MintRequest<String>,
    ) -> Result<MintResponse, Error> {
        let mut st = self.state.lock().unwrap();
        let q = st
            .mint_quotes
            .get_mut(&request.quote)
            .ok_or(Error::UnknownQuote)?;
        match q.state {
            MintQuoteState::Unpaid => return Err(Error::UnpaidQuote),
            MintQuoteState::Issued => return Err(Error::IssuedQuote),
            MintQuoteState::Paid => {}
        }
        let out: u64 = request.outputs.iter().map(|o| o.amount.to_u64()).sum();
        if out != q.amount {
            return Err(Error::TransactionUnbalanced(q.amount, out, 0));
        }
        let signatures = self.sign(&request.outputs)?;
        q.state = MintQuoteState::Issued;
        Ok(MintResponse { signatures })
    }

    async fn post_batch_check_mint_quote_status(
        &self,
        _method: &PaymentMethod,
        _request: BatchCheckMintQuoteRequest<String>,
    ) -> Result<Vec<MintQuoteResponse<String>>, Error> {
        Err(Error::UnsupportedPaymentMethod) // NUT-29 not advertised
    }

    async fn post_batch_mint(
        &self,
        _method: &PaymentMethod,
        _request: BatchMintRequest<String>,
    ) -> Result<MintResponse, Error> {
        Err(Error::UnsupportedPaymentMethod)
    }

    async fn post_melt_quote(
        &self,
        request: MeltQuoteRequest,
    ) -> Result<MeltQuoteCreateResponse<String>, Error> {
        let MeltQuoteRequest::Bolt11(req) = request else {
            return Err(Error::UnsupportedPaymentMethod);
        };
        let msat = req
            .request
            .amount_milli_satoshis()
            .ok_or(Error::InvoiceAmountUndefined)?;
        let mut st = self.state.lock().unwrap();
        let id = Self::next_id(&mut st, "meltq");
        let q = MeltQ {
            amount: msat / 1000,
            request: req.request.to_string(),
            state: MeltQuoteState::Unpaid,
        };
        let resp = Self::melt_quote_response(&id, &q, None);
        st.melt_quotes.insert(id, q);
        Ok(MeltQuoteCreateResponse::Bolt11(resp))
    }

    async fn get_mint_quote_status(
        &self,
        _method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MintQuoteResponse<String>, Error> {
        let st = self.state.lock().unwrap();
        let q = st.mint_quotes.get(quote_id).ok_or(Error::UnknownQuote)?;
        Ok(MintQuoteResponse::Bolt11(Self::mint_quote_response(
            quote_id, q,
        )))
    }

    async fn get_melt_quote_status(
        &self,
        _method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        let st = self.state.lock().unwrap();
        let q = st.melt_quotes.get(quote_id).ok_or(Error::UnknownQuote)?;
        Ok(MeltQuoteResponse::Bolt11(Self::melt_quote_response(
            quote_id, q, None,
        )))
    }

    async fn post_melt(
        &self,
        _method: &PaymentMethod,
        request: MeltRequest<String>,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        let mut st = self.state.lock().unwrap();
        let (amount, invoice) = {
            let q = st
                .melt_quotes
                .get(request.quote())
                .ok_or(Error::UnknownQuote)?;
            if q.state == MeltQuoteState::Paid {
                return Err(Error::RequestAlreadyPaid);
            }
            (q.amount, q.request.clone())
        };
        let (total, ys) = self.verify_inputs(&st, request.inputs())?;
        if total < amount + FEE_RESERVE {
            return Err(Error::TransactionUnbalanced(total, amount, FEE_RESERVE));
        }
        st.spent.extend(ys);

        // NUT-08 change: split the overpayment over the blank outputs in order.
        let mut change_left = total - amount - FEE_PAID;
        let mut change = Vec::new();
        if let Some(outputs) = request.outputs() {
            for o in outputs {
                if change_left == 0 {
                    break;
                }
                let part = 1u64 << (63 - change_left.leading_zeros());
                let mut filled = o.clone();
                filled.amount = Amount::from(part);
                change.extend(self.sign(&[filled])?);
                change_left -= part;
            }
        }

        let q = st
            .melt_quotes
            .get_mut(request.quote())
            .expect("checked above");
        q.state = MeltQuoteState::Paid;
        self.paid_invoices.lock().unwrap().push(invoice);
        let resp = Self::melt_quote_response(request.quote(), q, Some(change));
        Ok(MeltQuoteResponse::Bolt11(resp))
    }

    async fn post_swap(&self, request: SwapRequest) -> Result<SwapResponse, Error> {
        let mut st = self.state.lock().unwrap();
        let (total, ys) = self.verify_inputs(&st, request.inputs())?;
        let out: u64 = request.outputs().iter().map(|o| o.amount.to_u64()).sum();
        if out != total {
            return Err(Error::TransactionUnbalanced(total, out, 0));
        }
        let signatures = self.sign(request.outputs())?;
        st.spent.extend(ys);
        Ok(SwapResponse { signatures })
    }

    async fn get_mint_info(&self) -> Result<MintInfo, Error> {
        let mint_settings = MintMethodSettings {
            method: PaymentMethod::BOLT11,
            unit: CurrencyUnit::Sat,
            method_name: None,
            min_amount: Some(Amount::from(1u64)),
            max_amount: Some(Amount::from(1_000_000u64)),
            options: Some(MintMethodOptions::Bolt11 { description: true }),
        };
        let melt_settings = MeltMethodSettings {
            method: PaymentMethod::BOLT11,
            unit: CurrencyUnit::Sat,
            method_name: None,
            min_amount: Some(Amount::from(1u64)),
            max_amount: Some(Amount::from(1_000_000u64)),
            options: None,
        };
        Ok(MintInfo::new().name("fake mint").nuts(
            Nuts::new()
                .nut04(NUT04Settings::new(vec![mint_settings], false))
                .nut05(NUT05Settings::new(vec![melt_settings], false))
                .nut07(true)
                .nut08(true),
        ))
    }

    async fn post_check_state(
        &self,
        request: CheckStateRequest,
    ) -> Result<CheckStateResponse, Error> {
        let st = self.state.lock().unwrap();
        Ok(CheckStateResponse {
            states: request
                .ys
                .into_iter()
                .map(|y| ProofState {
                    y,
                    state: if st.spent.contains(&y) {
                        State::Spent
                    } else {
                        State::Unspent
                    },
                    witness: None,
                })
                .collect(),
        })
    }

    async fn post_restore(&self, _request: RestoreRequest) -> Result<RestoreResponse, Error> {
        Ok(RestoreResponse {
            outputs: vec![],
            signatures: vec![],
        })
    }

    async fn get_auth_wallet(&self) -> Option<AuthWallet> {
        None
    }

    async fn set_auth_wallet(&self, _wallet: Option<AuthWallet>) {}
}
