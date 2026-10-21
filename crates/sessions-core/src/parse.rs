//! Turn whatever the user pasted or scanned into a [`PaymentRequest`].

use std::str::FromStr;

use crate::types::{PaymentRequest, Sats};
use crate::{Error, Result};

pub fn parse(input: &str) -> Result<PaymentRequest> {
    let s = input.trim();
    if s.is_empty() {
        return Err(Error::UnrecognisedRequest);
    }
    let lower = s.to_ascii_lowercase();

    // URI schemes first.
    if let Some(rest) = strip_scheme(&lower, s, "bitcoin:") {
        return parse_bip21(rest);
    }
    if let Some(rest) = strip_scheme(&lower, s, "liquidnetwork:") {
        return parse_liquid_uri(rest);
    }
    if let Some(rest) = strip_scheme(&lower, s, "lightning:") {
        return parse(rest);
    }
    if let Some(rest) = strip_scheme(&lower, s, "cashu:") {
        return parse(rest);
    }

    parse_bare(s).ok_or(Error::UnrecognisedRequest)
}

fn strip_scheme<'a>(lower: &str, original: &'a str, scheme: &str) -> Option<&'a str> {
    lower.starts_with(scheme).then(|| &original[scheme.len()..])
}

fn parse_bare(s: &str) -> Option<PaymentRequest> {
    let lower = s.to_ascii_lowercase();

    if lower.starts_with("cashua") || lower.starts_with("cashub") {
        return Some(PaymentRequest::CashuToken {
            token: s.to_string(),
        });
    }
    if is_bolt11(&lower) {
        return Some(PaymentRequest::Bolt11 { invoice: lower });
    }
    if lower.starts_with("lno1") {
        return Some(PaymentRequest::Bolt12 { offer: lower });
    }
    if lower.starts_with("lnurl1") {
        return Some(PaymentRequest::Lnurl { lnurl: lower });
    }
    if is_spark_address(&lower) {
        return Some(PaymentRequest::SparkAddress { address: lower });
    }
    if is_liquid_address(s) {
        return Some(PaymentRequest::LiquidAddress {
            address: s.to_string(),
            amount_sat: None,
        });
    }
    if is_bitcoin_address(s) {
        return Some(PaymentRequest::BitcoinAddress {
            address: s.to_string(),
            amount_sat: None,
            label: None,
            payjoin: None,
        });
    }
    if is_lightning_address(&lower) {
        return Some(PaymentRequest::LightningAddress { address: lower });
    }
    None
}

fn is_bolt11(lower: &str) -> bool {
    ["lnbc", "lntbs", "lntb", "lnbcrt", "lnsb"]
        .iter()
        .any(|p| lower.starts_with(p))
        && lower.len() > 20
}

fn is_spark_address(lower: &str) -> bool {
    // bech32m with Spark HRPs: sp (mainnet), spt (testnet), sps (signet), sprt (regtest).
    ["sp1", "spt1", "sps1", "sprt1"]
        .iter()
        .any(|p| lower.starts_with(p))
        && lower.len() > 40
}

fn is_liquid_address(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    // Confidential blech32 (lq1), unconfidential bech32 (ex1), testnet (tlq1/tex1),
    // and base58 confidential (VJL…, VT…).
    ["lq1", "ex1", "tlq1", "tex1"]
        .iter()
        .any(|p| lower.starts_with(p))
        || ((s.starts_with("VJL") || s.starts_with("VT")) && s.len() > 60)
}

fn is_bitcoin_address(s: &str) -> bool {
    bitcoin::Address::from_str(s).is_ok()
}

fn is_lightning_address(lower: &str) -> bool {
    let mut parts = lower.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(user), Some(domain), None) => {
            !user.is_empty()
                && domain.contains('.')
                && user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))
        }
        _ => false,
    }
}

/// Amount encoded in a BOLT11 invoice's human-readable part, rounded down to
/// whole sats. `None` for amountless invoices.
pub fn bolt11_amount_sat(invoice: &str) -> Option<Sats> {
    let lower = invoice.to_ascii_lowercase();
    let hrp = &lower[..lower.rfind('1')?];
    let rest = ["lnbcrt", "lntbs", "lntb", "lnbc", "lnsb"]
        .iter()
        .find_map(|p| hrp.strip_prefix(p))?;
    if rest.is_empty() {
        return None;
    }
    let (digits, mult) = match rest.chars().last()? {
        c @ ('m' | 'u' | 'n' | 'p') => (&rest[..rest.len() - 1], Some(c)),
        _ => (rest, None),
    };
    let n: u64 = digits.parse().ok()?;
    // Convert to millisats first so 'p' amounts are exact.
    let msat = match mult {
        None => n.checked_mul(100_000_000_000)?,
        Some('m') => n.checked_mul(100_000_000)?,
        Some('u') => n.checked_mul(100_000)?,
        Some('n') => n.checked_mul(100)?,
        Some('p') => n / 10,
        Some(_) => return None,
    };
    Some(msat / 1000)
}

fn btc_to_sats(v: &str) -> Option<Sats> {
    // Exact decimal parse; avoid floats with money.
    let (whole, frac) = v.split_once('.').unwrap_or((v, ""));
    if frac.len() > 8 || whole.is_empty() && frac.is_empty() {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac_padded = format!("{frac:0<8}");
    let frac: u64 = frac_padded.parse().ok()?;
    whole.checked_mul(100_000_000)?.checked_add(frac)
}

fn query_pairs(q: &str) -> impl Iterator<Item = (String, String)> + '_ {
    q.split('&').filter_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        Some((k.to_ascii_lowercase(), percent_decode(v)))
    })
}

fn percent_decode(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&v[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_bip21(rest: &str) -> Result<PaymentRequest> {
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut amount = None;
    let mut label = None;
    let mut extras = Vec::new();
    let mut payjoin = None;

    for (k, v) in query_pairs(query) {
        match k.as_str() {
            "amount" => amount = btc_to_sats(&v),
            // BIP78/77: keep the whole URI; the payjoin library parses it.
            // BIP77 URIs are often upper-case for denser QR codes.
            "pj" if {
                let l = v.to_ascii_lowercase();
                l.starts_with("https://") || (l.starts_with("http://") && l.contains(".onion"))
            } =>
            {
                payjoin = Some(format!("bitcoin:{rest}"))
            }
            "label" | "message" => label = label.or(Some(v)),
            "lightning" | "lno" | "spark" => {
                if let Some(r) = parse_bare(&v) {
                    extras.push(r);
                }
            }
            _ => {}
        }
    }

    // Preference order: Spark (free, instant) > BOLT12 > BOLT11 > on-chain.
    extras.sort_by_key(|r| match r {
        PaymentRequest::SparkAddress { .. } => 0,
        PaymentRequest::Bolt12 { .. } => 1,
        PaymentRequest::Bolt11 { .. } => 2,
        _ => 3,
    });

    let mut options = extras;
    if !addr.is_empty() {
        if !is_bitcoin_address(addr) {
            return Err(Error::UnrecognisedRequest);
        }
        options.push(PaymentRequest::BitcoinAddress {
            address: addr.to_string(),
            amount_sat: amount,
            label,
            payjoin,
        });
    }

    match options.len() {
        0 => Err(Error::UnrecognisedRequest),
        1 => Ok(options.remove(0)),
        _ => Ok(PaymentRequest::Unified { options }),
    }
}

fn parse_liquid_uri(rest: &str) -> Result<PaymentRequest> {
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !is_liquid_address(addr) {
        return Err(Error::UnrecognisedRequest);
    }
    let amount = query_pairs(query)
        .find(|(k, _)| k == "amount")
        .and_then(|(_, v)| btc_to_sats(&v));
    Ok(PaymentRequest::LiquidAddress {
        address: addr.to_string(),
        amount_sat: amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";

    #[test]
    fn plain_address() {
        assert!(matches!(
            parse(ADDR).unwrap(),
            PaymentRequest::BitcoinAddress { .. }
        ));
    }

    #[test]
    fn bip21_amount_is_exact() {
        let r = parse(&format!(
            "bitcoin:{ADDR}?amount=0.00012345&label=Coffee%20shop"
        ))
        .unwrap();
        match r {
            PaymentRequest::BitcoinAddress {
                amount_sat, label, ..
            } => {
                assert_eq!(amount_sat, Some(12_345));
                assert_eq!(label.as_deref(), Some("Coffee shop"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unified_qr_prefers_lightning() {
        let inv = "lnbc10u1pjexampleexampleexampleexampleexample";
        let r = parse(&format!(
            "BITCOIN:{}?amount=0.00001&lightning={}",
            ADDR.to_uppercase(),
            inv.to_uppercase()
        ));
        // Uppercase addresses (QR alphanumeric mode) must still parse.
        let r = r.unwrap();
        let opts = r.options();
        assert!(matches!(opts[0], PaymentRequest::Bolt11 { .. }));
        assert!(matches!(opts[1], PaymentRequest::BitcoinAddress { .. }));
        assert_eq!(r.amount_sat(), Some(1_000));
    }

    #[test]
    fn payjoin_uri_is_kept() {
        let uri = format!("bitcoin:{ADDR}?amount=0.001&pj=HTTPS://PAYJO.IN/ABC%23RK1Q");
        match parse(&uri).unwrap() {
            PaymentRequest::BitcoinAddress {
                payjoin: Some(pj),
                amount_sat,
                ..
            } => {
                assert_eq!(pj, uri);
                assert_eq!(amount_sat, Some(100_000));
            }
            other => panic!("{other:?}"),
        }
        // A plain BIP21 has no payjoin.
        assert!(matches!(
            parse(&format!("bitcoin:{ADDR}?amount=0.001")).unwrap(),
            PaymentRequest::BitcoinAddress { payjoin: None, .. }
        ));
    }

    #[test]
    fn misc_formats() {
        assert!(matches!(
            parse("ben@btcsessions.ca").unwrap(),
            PaymentRequest::LightningAddress { .. }
        ));
        assert!(matches!(
            parse("cashuBo2FteCJodHRwczovL21pbnQ").unwrap(),
            PaymentRequest::CashuToken { .. }
        ));
        assert!(matches!(
            parse("lightning:LNURL1DP68GURN8GHJ7").unwrap(),
            PaymentRequest::Lnurl { .. }
        ));
        assert!(parse("hello world").is_err());
    }

    #[test]
    fn bolt11_amounts() {
        assert_eq!(
            bolt11_amount_sat("lnbc2500u1pvjluezpp5qqqsyqcyq5"),
            Some(250_000)
        );
        assert_eq!(
            bolt11_amount_sat("lnbc20m1pvjluezpp5qqqsyqcyq5"),
            Some(2_000_000)
        );
        assert_eq!(bolt11_amount_sat("lnbc10n1pjexample"), Some(1));
        assert_eq!(bolt11_amount_sat("lnbc1pvjluezpp5qqqsyqcyq5"), None);
        assert_eq!(bolt11_amount_sat("lntbs100u1pexample"), Some(10_000));
    }

    #[test]
    fn sats_conversion() {
        assert_eq!(btc_to_sats("1"), Some(100_000_000));
        assert_eq!(btc_to_sats("0.1"), Some(10_000_000));
        assert_eq!(btc_to_sats(".00000001"), Some(1));
        assert_eq!(btc_to_sats("0.000000001"), None);
    }
}
