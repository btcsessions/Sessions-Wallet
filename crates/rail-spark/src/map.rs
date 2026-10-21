//! Pure translation between Breez SDK types and `sessions_core` types.
//!
//! Nothing here touches the network, so all of it is unit-tested.

use breez_sdk_spark::{
    BitcoinNetwork, FeePolicy, Network as SdkNetwork, OnchainConfirmationSpeed,
    Payment as SdkPayment, PaymentDetails, PaymentMethod as SdkMethod, PaymentStatus as SdkStatus,
    PaymentType,
};
use serde::{Deserialize, Serialize};
use sessions_core::{
    AccountId, Direction, Error, FeePriority, Method, Network, Payment, PaymentRequest,
    PaymentStatus, Result, Sats,
};

pub(crate) const RAIL: &str = "spark";

/// Methods the SDK can produce a receive target for.
pub const RECEIVES: &[Method] = &[Method::SparkAddress, Method::Bolt11, Method::BitcoinAddress];

/// Methods the SDK can pay. Bolt12 is deliberately absent: 0.26.1 parses
/// offers but `prepare_send_payment` rejects them as unsupported.
pub const PAYS: &[Method] = &[
    Method::SparkAddress,
    Method::Bolt11,
    Method::LightningAddress,
    Method::Lnurl,
    Method::BitcoinAddress,
];

pub(crate) fn sdk_network(network: Network) -> Result<SdkNetwork> {
    match network {
        Network::Bitcoin => Ok(SdkNetwork::Mainnet),
        Network::Regtest => Ok(SdkNetwork::Regtest),
        other => Err(Error::Network(format!(
            "Spark accounts run on mainnet or regtest only, not {other:?}"
        ))),
    }
}

/// The network the SDK's parser reports for addresses/invoices on `network`.
pub(crate) fn parsed_network(network: SdkNetwork) -> BitcoinNetwork {
    network.into()
}

pub(crate) fn status(s: SdkStatus) -> PaymentStatus {
    match s {
        SdkStatus::Completed => PaymentStatus::Complete,
        SdkStatus::Pending => PaymentStatus::Pending,
        SdkStatus::Failed => PaymentStatus::Failed,
    }
}

pub(crate) fn direction(t: PaymentType) -> Direction {
    match t {
        PaymentType::Send => Direction::Outgoing,
        PaymentType::Receive => Direction::Incoming,
    }
}

/// The method a payment moved over, preferring the detailed record.
pub(crate) fn method(p: &SdkPayment) -> Method {
    match &p.details {
        Some(PaymentDetails::Lightning {
            lnurl_pay_info: Some(info),
            ..
        }) => {
            if info.ln_address.is_some() {
                Method::LightningAddress
            } else {
                Method::Lnurl
            }
        }
        Some(PaymentDetails::Lightning { .. }) => Method::Bolt11,
        Some(PaymentDetails::Spark { .. } | PaymentDetails::Token { .. }) => Method::SparkAddress,
        Some(PaymentDetails::Withdraw { .. } | PaymentDetails::Deposit { .. }) => {
            Method::BitcoinAddress
        }
        None => match p.method {
            SdkMethod::Lightning => Method::Bolt11,
            SdkMethod::Deposit | SdkMethod::Withdraw => Method::BitcoinAddress,
            // `Unknown` covers Spark-internal transfer types (e.g. counter swaps),
            // so a Spark transfer is the closest honest label.
            SdkMethod::Spark | SdkMethod::Token | SdkMethod::Unknown => Method::SparkAddress,
        },
    }
}

pub(crate) fn description(p: &SdkPayment) -> Option<String> {
    match &p.details {
        Some(PaymentDetails::Lightning {
            description,
            lnurl_pay_info,
            ..
        }) => description.clone().filter(|d| !d.is_empty()).or_else(|| {
            lnurl_pay_info
                .as_ref()
                .and_then(|i| i.extract_description())
        }),
        Some(PaymentDetails::Spark {
            invoice_details: Some(d),
            ..
        }) => d.description.clone(),
        _ => None,
    }
}

pub(crate) fn sats(v: u128) -> Result<Sats> {
    Sats::try_from(v).map_err(|_| Error::rail(RAIL, format!("amount {v} does not fit in sats")))
}

/// Map a bitcoin-denominated SDK payment. Token payments must be filtered out
/// before calling this: their amounts are token base units, not sats.
pub(crate) fn payment(account: &AccountId, p: &SdkPayment) -> Result<Payment> {
    Ok(Payment {
        id: p.id.clone(),
        account: account.clone(),
        direction: direction(p.payment_type),
        amount_sat: sats(p.amount)?,
        fee_sat: sats(p.fees)?,
        status: status(p.status),
        method: method(p),
        timestamp: p.timestamp,
        description: description(p),
        token: None,
    })
}

pub(crate) fn confirmation_speed(fee: FeePriority) -> Result<OnchainConfirmationSpeed> {
    match fee {
        FeePriority::Fast => Ok(OnchainConfirmationSpeed::Fast),
        FeePriority::Normal => Ok(OnchainConfirmationSpeed::Medium),
        FeePriority::Slow => Ok(OnchainConfirmationSpeed::Slow),
        FeePriority::Custom(_) => Err(Error::Unsupported(
            "set a custom fee rate on a Spark withdrawal (choose fast, normal or slow)".into(),
        )),
    }
}

/// Split what the SDK quoted into (amount reaching the payee, fee).
/// Under `FeesIncluded` the SDK's amount is the total spend.
pub(crate) fn split(amount: Sats, fee: Sats, policy: FeePolicy) -> Result<(Sats, Sats)> {
    match policy {
        FeePolicy::FeesExcluded => Ok((amount, fee)),
        FeePolicy::FeesIncluded => amount
            .checked_sub(fee)
            .filter(|a| *a > 0)
            .map(|a| (a, fee))
            .ok_or_else(|| {
                Error::rail(
                    RAIL,
                    format!("{amount} sats does not cover the {fee} sat fee"),
                )
            }),
    }
}

/// The concrete request to pay and the string the SDK should parse for it.
pub(crate) fn destination(req: &PaymentRequest) -> Result<(Method, String)> {
    if let PaymentRequest::Unified { options } = req {
        return options
            .iter()
            .find_map(|o| destination(o).ok())
            .ok_or_else(|| Error::Unsupported("pay any of the options in this request".into()));
    }
    let input = match req {
        PaymentRequest::BitcoinAddress { address, .. } => address.clone(),
        PaymentRequest::Bolt11 { invoice } => invoice.clone(),
        PaymentRequest::SparkAddress { address } => address.clone(),
        PaymentRequest::LightningAddress { address } => address.clone(),
        PaymentRequest::Lnurl { lnurl } => lnurl.clone(),
        PaymentRequest::Bolt12 { .. } => {
            return Err(Error::Unsupported(
                "pay BOLT12 offers (not supported by the Spark SDK yet)".into(),
            ))
        }
        PaymentRequest::LiquidAddress { .. } => {
            return Err(Error::Unsupported("pay Liquid addresses".into()))
        }
        PaymentRequest::CashuToken { .. } => {
            return Err(Error::Unsupported("pay Cashu tokens".into()))
        }
        PaymentRequest::Unified { .. } => unreachable!("handled above"),
    };
    let method = req.method().ok_or(Error::UnrecognisedRequest)?;
    Ok((method, input))
}

/// What `SendQuote.handle` carries. The prepared SDK response itself stays in
/// memory (it is not deserializable), so a handle is single-use and does not
/// survive a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuoteHandle {
    pub rail: String,
    pub account: AccountId,
    pub id: String,
}

impl QuoteHandle {
    pub(crate) fn new(account: AccountId, id: String) -> Self {
        Self {
            rail: RAIL.into(),
            account,
            id,
        }
    }

    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).expect("QuoteHandle always serializes")
    }

    pub(crate) fn decode(handle: &str, account: &AccountId) -> Result<Self> {
        let h: QuoteHandle = serde_json::from_str(handle)
            .map_err(|_| Error::rail(RAIL, "this quote was not made by a Spark account"))?;
        if h.rail != RAIL || &h.account != account {
            return Err(Error::rail(
                RAIL,
                "this quote belongs to a different account",
            ));
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use breez_sdk_spark::{
        LnurlPayInfo, SparkHtlcDetails, SparkHtlcStatus, SparkInvoicePaymentDetails,
    };

    fn sdk_payment(details: Option<PaymentDetails>, method: SdkMethod) -> SdkPayment {
        SdkPayment {
            id: "p1".into(),
            payment_type: PaymentType::Send,
            status: SdkStatus::Completed,
            amount: 1_000,
            fees: 3,
            timestamp: 1_760_000_000,
            method,
            details,
            conversion_details: None,
        }
    }

    fn lightning(description: Option<&str>, lnurl: Option<LnurlPayInfo>) -> PaymentDetails {
        PaymentDetails::Lightning {
            description: description.map(Into::into),
            invoice: "lnbc1".into(),
            destination_pubkey: "02ab".into(),
            htlc_details: SparkHtlcDetails {
                payment_hash: "00".into(),
                preimage: None,
                expiry_time: 0,
                status: SparkHtlcStatus::PreimageShared,
            },
            lnurl_pay_info: lnurl,
            lnurl_withdraw_info: None,
            lnurl_receive_metadata: None,
            conversion_info: None,
        }
    }

    #[test]
    fn networks() {
        assert_eq!(sdk_network(Network::Bitcoin).unwrap(), SdkNetwork::Mainnet);
        assert_eq!(sdk_network(Network::Regtest).unwrap(), SdkNetwork::Regtest);
        assert!(matches!(
            sdk_network(Network::Testnet),
            Err(Error::Network(_))
        ));
        assert!(matches!(
            sdk_network(Network::Signet),
            Err(Error::Network(_))
        ));
        assert_eq!(parsed_network(SdkNetwork::Mainnet), BitcoinNetwork::Bitcoin);
        assert_eq!(parsed_network(SdkNetwork::Regtest), BitcoinNetwork::Regtest);
    }

    #[test]
    fn statuses_and_directions() {
        assert_eq!(status(SdkStatus::Completed), PaymentStatus::Complete);
        assert_eq!(status(SdkStatus::Pending), PaymentStatus::Pending);
        assert_eq!(status(SdkStatus::Failed), PaymentStatus::Failed);
        assert_eq!(direction(PaymentType::Send), Direction::Outgoing);
        assert_eq!(direction(PaymentType::Receive), Direction::Incoming);
    }

    #[test]
    fn methods_from_details() {
        let ln_addr = LnurlPayInfo {
            ln_address: Some("a@b.c".into()),
            ..Default::default()
        };
        let lnurl = LnurlPayInfo::default();
        let cases = [
            (Some(lightning(None, None)), Method::Bolt11),
            (
                Some(lightning(None, Some(ln_addr))),
                Method::LightningAddress,
            ),
            (Some(lightning(None, Some(lnurl))), Method::Lnurl),
            (
                Some(PaymentDetails::Spark {
                    invoice_details: None,
                    htlc_details: None,
                    conversion_info: None,
                }),
                Method::SparkAddress,
            ),
            (
                Some(PaymentDetails::Withdraw { tx_id: "t".into() }),
                Method::BitcoinAddress,
            ),
            (
                Some(PaymentDetails::Deposit {
                    tx_id: "t".into(),
                    vout: 0,
                }),
                Method::BitcoinAddress,
            ),
        ];
        for (details, want) in cases {
            assert_eq!(method(&sdk_payment(details, SdkMethod::Unknown)), want);
        }
    }

    #[test]
    fn methods_without_details() {
        for (m, want) in [
            (SdkMethod::Lightning, Method::Bolt11),
            (SdkMethod::Spark, Method::SparkAddress),
            (SdkMethod::Deposit, Method::BitcoinAddress),
            (SdkMethod::Withdraw, Method::BitcoinAddress),
            (SdkMethod::Unknown, Method::SparkAddress),
        ] {
            assert_eq!(method(&sdk_payment(None, m)), want);
        }
    }

    #[test]
    fn descriptions() {
        let p = sdk_payment(Some(lightning(Some("coffee"), None)), SdkMethod::Lightning);
        assert_eq!(description(&p).as_deref(), Some("coffee"));

        let info = LnurlPayInfo {
            metadata: Some(r#"[["text/plain","tip jar"]]"#.into()),
            ..Default::default()
        };
        let p = sdk_payment(Some(lightning(Some(""), Some(info))), SdkMethod::Lightning);
        assert_eq!(description(&p).as_deref(), Some("tip jar"));

        let p = sdk_payment(
            Some(PaymentDetails::Spark {
                invoice_details: Some(SparkInvoicePaymentDetails {
                    description: Some("rent".into()),
                    invoice: "x".into(),
                }),
                htlc_details: None,
                conversion_info: None,
            }),
            SdkMethod::Spark,
        );
        assert_eq!(description(&p).as_deref(), Some("rent"));
        assert_eq!(description(&sdk_payment(None, SdkMethod::Deposit)), None);
    }

    #[test]
    fn full_payment_mapping() {
        let mut p = sdk_payment(Some(lightning(Some("x"), None)), SdkMethod::Lightning);
        p.payment_type = PaymentType::Receive;
        p.status = SdkStatus::Pending;
        let m = payment(&"spend".into(), &p).unwrap();
        assert_eq!(m.account, AccountId::from("spend"));
        assert_eq!(
            (m.amount_sat, m.fee_sat, m.timestamp),
            (1_000, 3, 1_760_000_000)
        );
        assert_eq!(m.direction, Direction::Incoming);
        assert_eq!(m.status, PaymentStatus::Pending);
        assert_eq!(m.method, Method::Bolt11);

        p.amount = u128::from(u64::MAX) + 1;
        assert!(payment(&"spend".into(), &p).is_err());
    }

    #[test]
    fn fee_priorities() {
        assert!(matches!(
            confirmation_speed(FeePriority::Fast).unwrap(),
            OnchainConfirmationSpeed::Fast
        ));
        assert!(matches!(
            confirmation_speed(FeePriority::Normal).unwrap(),
            OnchainConfirmationSpeed::Medium
        ));
        assert!(matches!(
            confirmation_speed(FeePriority::Slow).unwrap(),
            OnchainConfirmationSpeed::Slow
        ));
        assert!(matches!(
            confirmation_speed(FeePriority::Custom(5)),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn fee_split() {
        assert_eq!(
            split(1_000, 10, FeePolicy::FeesExcluded).unwrap(),
            (1_000, 10)
        );
        assert_eq!(
            split(1_000, 10, FeePolicy::FeesIncluded).unwrap(),
            (990, 10)
        );
        assert!(split(10, 10, FeePolicy::FeesIncluded).is_err());
        assert!(split(5, 10, FeePolicy::FeesIncluded).is_err());
    }

    #[test]
    fn destinations() {
        let (m, s) = destination(&PaymentRequest::LightningAddress {
            address: "a@b.c".into(),
        })
        .unwrap();
        assert_eq!((m, s.as_str()), (Method::LightningAddress, "a@b.c"));
        assert!(matches!(
            destination(&PaymentRequest::Bolt12 {
                offer: "lno1".into()
            }),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            destination(&PaymentRequest::LiquidAddress {
                address: "lq1".into(),
                amount_sat: None
            }),
            Err(Error::Unsupported(_))
        ));
        let unified = PaymentRequest::Unified {
            options: vec![
                PaymentRequest::LiquidAddress {
                    address: "lq1".into(),
                    amount_sat: None,
                },
                PaymentRequest::Bolt11 {
                    invoice: "lnbc1x".into(),
                },
                PaymentRequest::BitcoinAddress {
                    address: "bc1q".into(),
                    amount_sat: None,
                    label: None,
                    payjoin: None,
                },
            ],
        };
        assert_eq!(
            destination(&unified).unwrap(),
            (Method::Bolt11, "lnbc1x".into())
        );
        assert!(destination(&PaymentRequest::Unified { options: vec![] }).is_err());
    }

    #[test]
    fn method_lists_exclude_unsupported() {
        assert!(!PAYS.contains(&Method::Bolt12));
        assert!(!RECEIVES.contains(&Method::Bolt12));
        assert!(!PAYS.contains(&Method::LiquidAddress));
    }

    #[test]
    fn quote_handle_round_trip() {
        let acct = AccountId::from("spend");
        let h = QuoteHandle::new(acct.clone(), "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into());
        assert_eq!(QuoteHandle::decode(&h.encode(), &acct).unwrap(), h);

        assert!(QuoteHandle::decode(&h.encode(), &"other".into()).is_err());
        assert!(QuoteHandle::decode("not json", &acct).is_err());
        let foreign = QuoteHandle {
            rail: "onchain".into(),
            ..h
        };
        assert!(QuoteHandle::decode(&foreign.encode(), &acct).is_err());
    }
}
