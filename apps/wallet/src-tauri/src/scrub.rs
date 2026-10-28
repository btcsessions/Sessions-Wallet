//! Take payment details out of log lines before they're kept for "Copy
//! diagnostic log": the wallet's own lines never hold them, but SDK lines
//! can (`amount_out=…`, `dest=…`, invoices, addresses).

/// Keys whose values are left out.
const SENSITIVE: &[&str] = &[
    "amount", "sat", "msat", "dest", "destination", "invoice", "bolt11", "bolt12", "address", "addr",
    "payment_request", "preimage", "payment_hash", "txid", "lnurl", "offer", "pubkey", "node_id",
];

const HIDDEN: &str = "[hidden]";

/// Something that is itself a payment detail.
fn sensitive_value(v: &str) -> bool {
    let t = v.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
    ["bc1", "tb1", "bcrt1", "lnbc", "lntb", "lnbcrt", "lnurl", "lno1", "lni1", "cashu"].iter().any(|p| t.starts_with(p) && t.len() > 20)
        || (t.len() >= 40 && t.chars().all(|c| c.is_ascii_hexdigit()))
}

pub fn scrub(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for (i, word) in line.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if let Some((k, v)) = word.split_once(['=', ':']) {
            let key = k.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_').to_ascii_lowercase();
            if !v.is_empty() && SENSITIVE.iter().any(|s| key == *s || key.ends_with(&format!("_{s}")) || key.starts_with(&format!("{s}_"))) {
                out.push_str(k);
                out.push_str(&word[k.len()..k.len() + 1]);
                out.push_str(HIDDEN);
                continue;
            }
        }
        if sensitive_value(word) {
            out.push_str(HIDDEN);
        } else {
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn hides_payment_details() {
        let s = scrub("send ok dest=03abc amount_out=5000 fee_sat=12 to bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
        assert!(!s.contains("03abc") && !s.contains("5000") && !s.contains("bc1q"), "{s}");
        assert!(s.contains("send ok") && s.contains("dest=[hidden]"));
        let s = scrub("paying lnbc10u1p3xyzxyzxyzxyzxyzxyzxyzxyz now");
        assert!(!s.contains("lnbc"), "{s}");
        let s = scrub(&format!("tx {} broadcast", "ab".repeat(32)));
        assert!(!s.contains(&"ab".repeat(32)));
        // Ordinary lines stay readable.
        assert_eq!(scrub("jade: link dropped since the last call"), "jade: link dropped since the last call");
        assert_eq!(scrub("sync: 2 relays answered"), "sync: 2 relays answered");
    }
}
