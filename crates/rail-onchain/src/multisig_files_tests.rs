//! Tests for multisig wallet files. Real files come from the BIP-129 spec,
//! Caravan's and BlueWallet's test fixtures, Nunchuk and Sparrow exports.

use super::*;
use crate::descriptors::key_expression;
use crate::multisig::bip48_path;
use sessions_core::keystore::Seed;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const ZOO: &str = "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong";

fn key(phrase: &str, network: Network, path: &str) -> Cosigner {
    let master = Seed::from_phrase(phrase)
        .unwrap()
        .master_xpriv(network)
        .unwrap();
    Cosigner::parse(&key_expression(&master, path).unwrap(), network).unwrap()
}

/// 2-of-3 BIP48 P2WSH from three different seeds.
fn two_of_three(network: Network) -> MultisigConfig {
    let p = bip48_path(network, 0);
    let mut c = MultisigConfig::sorted_p2wsh(
        2,
        vec![
            key(ABANDON, network, &p),
            key(LEGAL, network, &p),
            key(ZOO, network, &p),
        ],
    );
    c.name = Some("Family Vault".into());
    c
}

/// 3-of-5 where two seeds bring a second account.
fn three_of_five(network: Network) -> MultisigConfig {
    let p0 = bip48_path(network, 0);
    let p1 = bip48_path(network, 1);
    let mut c = MultisigConfig::sorted_p2wsh(
        3,
        vec![
            key(ABANDON, network, &p0),
            key(LEGAL, network, &p0),
            key(ZOO, network, &p0),
            key(ABANDON, network, &p1),
            key(LEGAL, network, &p1),
        ],
    );
    c.name = Some("Company treasury".into());
    c
}

fn labelled(mut c: MultisigConfig) -> MultisigConfig {
    for (i, k) in c.cosigners.iter_mut().enumerate() {
        k.label = Some(format!("Signer {}", i + 1));
    }
    c
}

fn without_names(mut c: MultisigConfig) -> MultisigConfig {
    c.name = None;
    for k in &mut c.cosigners {
        k.label = None;
    }
    c
}

fn change_address(c: &MultisigConfig, network: Network, index: u32) -> String {
    let pair = c.descriptors().unwrap();
    let flipped = DescriptorPair {
        external: pair.internal,
        internal: pair.external,
    };
    descriptors::address_at(&flipped, network, index).unwrap()
}

fn receive_address(c: &MultisigConfig, network: Network, index: u32) -> String {
    descriptors::address_at(&c.descriptors().unwrap(), network, index).unwrap()
}

/// What survives a round trip through each format.
fn expected_after(format: ExportFormat, c: &MultisigConfig) -> MultisigConfig {
    let mut want = c.clone();
    match format {
        ExportFormat::Descriptor | ExportFormat::Bsms => want = without_names(want),
        ExportFormat::Coldcard => {
            for k in &mut want.cosigners {
                k.label = None;
            }
            want.name = Some(coldcard_name(c.name.as_deref()));
        }
        // These files always carry names; we fill in plain ones.
        ExportFormat::Caravan | ExportFormat::Specter => {
            want.name.get_or_insert_with(|| "Multisig".into());
            for (i, k) in want.cosigners.iter_mut().enumerate() {
                k.label.get_or_insert_with(|| format!("Key {}", i + 1));
            }
        }
    }
    want
}

fn round_trip_all(c: &MultisigConfig, network: Network) {
    for format in ExportFormat::ALL {
        let file = c.export(format, network).unwrap();
        let back = MultisigConfig::import(&file.content, network)
            .unwrap_or_else(|e| panic!("{format:?} didn't read back: {e}\n{}", file.content));
        assert_eq!(
            back,
            expected_after(format, c),
            "{format:?}\n{}",
            file.content
        );
        assert_eq!(
            back.first_address(network).unwrap(),
            c.first_address(network).unwrap()
        );
    }
}

#[test]
fn round_trips_every_format_mainnet_and_testnet() {
    for network in [Network::Bitcoin, Network::Testnet] {
        round_trip_all(&labelled(two_of_three(network)), network);
        round_trip_all(&labelled(three_of_five(network)), network);
        // Unlabelled and unnamed too.
        round_trip_all(&without_names(two_of_three(network)), network);
    }
}

#[test]
fn round_trips_nested_legacy_and_mixed_paths() {
    let n = Network::Bitcoin;
    // Nested segwit, BIP48 `1'`.
    let p = "m/48'/0'/0'/1'";
    let mut nested = MultisigConfig::sorted_p2wsh(
        2,
        vec![key(ABANDON, n, p), key(LEGAL, n, p), key(ZOO, n, p)],
    );
    nested.script = ScriptType::P2shP2wsh;
    nested.name = Some("Nested".into());
    round_trip_all(&labelled(nested.clone()), n);
    assert!(nested.first_address(n).unwrap().starts_with('3'));
    // Legacy P2SH, BIP45.
    let mut legacy =
        MultisigConfig::sorted_p2wsh(2, vec![key(ABANDON, n, "m/45'"), key(LEGAL, n, "m/45'")]);
    legacy.script = ScriptType::P2sh;
    round_trip_all(&labelled(legacy), n);
    // Keys at different paths.
    let mixed = MultisigConfig::sorted_p2wsh(
        2,
        vec![
            key(ABANDON, n, "m/48'/0'/0'/2'"),
            key(LEGAL, n, "m/48'/0'/7'/2'"),
            key(ZOO, n, "m/48'/0'/0'/2'"),
        ],
    );
    round_trip_all(&labelled(mixed.clone()), n);
    let text = mixed.export(ExportFormat::Coldcard, n).unwrap().content;
    // One Derivation line per key (Keystone needs exactly that), each with
    // the comment form BlueWallet reads.
    assert_eq!(text.matches("\nDerivation: ").count(), 3);
    assert_eq!(text.matches("# derivation: ").count(), 3);
    assert!(text.contains("# derivation: m/48'/0'/7'/2'\nDerivation: m/48'/0'/7'/2'\n"));
}

#[test]
fn unsorted_multi_round_trips_where_the_format_allows() {
    let n = Network::Bitcoin;
    let mut c = labelled(two_of_three(n));
    c.policy = Policy::Multi {
        threshold: 2,
        sorted: false,
    };
    for format in [
        ExportFormat::Descriptor,
        ExportFormat::Bsms,
        ExportFormat::Specter,
    ] {
        let file = c.export(format, n).unwrap();
        assert_eq!(
            MultisigConfig::import(&file.content, n).unwrap(),
            expected_after(format, &c)
        );
    }
    // Coldcard text and Caravan always mean sortedmulti.
    assert!(c.export(ExportFormat::Coldcard, n).is_err());
    assert!(c.export(ExportFormat::Caravan, n).is_err());
    // Jade's `Sorted: False` is honoured on import.
    let text = two_of_three(n)
        .export(ExportFormat::Coldcard, n)
        .unwrap()
        .content
        .replace("Format: P2WSH\n", "Format: P2WSH\nSorted: False\n");
    let back = MultisigConfig::import(&text, n).unwrap();
    assert_eq!(back.policy, c.policy);
}

#[test]
fn export_files_look_like_the_originals() {
    let n = Network::Bitcoin;
    let c = labelled(two_of_three(n));
    let k = &c.cosigners;

    let cc = c.export(ExportFormat::Coldcard, n).unwrap();
    assert_eq!(cc.filename, "Family-Vault-multisig.txt");
    assert_eq!(cc.mime, "text/plain");
    let want = format!(
        "# Coldcard Multisig setup file (exported by Sessions Wallet)\n#\nName: Family Vault\nPolicy: 2 of 3\nDerivation: m/48'/0'/0'/2'\nFormat: P2WSH\n\n{}: {}\n{}: {}\n{}: {}\n",
        k[0].fingerprint.to_uppercase(),
        k[0].xpub,
        k[1].fingerprint.to_uppercase(),
        k[1].xpub,
        k[2].fingerprint.to_uppercase(),
        k[2].xpub,
    );
    assert_eq!(cc.content, want);
    // Sparrow splits on ':' and wants exactly two parts per line.
    for line in cc.content.lines().filter(|l| l.contains(':')) {
        assert_eq!(line.split(':').count(), 2, "{line}");
    }

    let d = c.export(ExportFormat::Descriptor, n).unwrap();
    assert_eq!(d.filename, "Family-Vault-descriptor.txt");
    let line = d.content.trim();
    assert_eq!(line, c.descriptor_with_checksum().unwrap());
    assert!(line.contains("/<0;1>/*") && line.starts_with("wsh(sortedmulti(2,"));
    // miniscript checks the checksum.
    Descriptor::<DescriptorPublicKey>::from_str(line).unwrap();
    assert!(Descriptor::<DescriptorPublicKey>::from_str(&line.replace("#", "#x")).is_err());

    let b = c.export(ExportFormat::Bsms, n).unwrap();
    assert_eq!(b.filename, "Family-Vault.bsms");
    let lines: Vec<&str> = b.content.lines().collect();
    assert_eq!(lines[0], "BSMS 1.0");
    assert!(lines[1].contains("/**,") && !lines[1].contains('#'));
    assert!(lines[1].contains(&format!("[{}/48'/0'/0'/2']", k[0].fingerprint)));
    assert_eq!(lines[2], "/0/*,/1/*");
    assert_eq!(lines[3], c.first_address(n).unwrap());

    let cv = c.export(ExportFormat::Caravan, n).unwrap();
    assert_eq!(cv.mime, "application/json");
    let v: Value = serde_json::from_str(&cv.content).unwrap();
    assert_eq!(v["addressType"], "P2WSH");
    assert_eq!(v["network"], "mainnet");
    assert_eq!(v["client"]["type"], "public");
    assert_eq!(v["quorum"]["requiredSigners"], 2);
    assert_eq!(v["quorum"]["totalSigners"], 3);
    assert_eq!(v["startingAddressIndex"], 0);
    assert_eq!(v["extendedPublicKeys"][1]["bip32Path"], "m/48'/0'/0'/2'");
    assert_eq!(v["extendedPublicKeys"][1]["xfp"], k[1].fingerprint);
    assert_eq!(v["extendedPublicKeys"][1]["method"], "text");
    assert_eq!(v["extendedPublicKeys"][1]["name"], "Signer 2");

    let sp = c.export(ExportFormat::Specter, n).unwrap();
    let v: Value = serde_json::from_str(&sp.content).unwrap();
    assert_eq!(v["label"], "Family Vault");
    assert_eq!(v["blockheight"], 0);
    let desc = v["descriptor"].as_str().unwrap();
    assert!(desc.contains("/0/*") && !desc.contains("<0;1>"));
    Descriptor::<DescriptorPublicKey>::from_str(desc).unwrap();
    assert_eq!(v["devices"][2]["label"], "Signer 3");

    // Testnet Caravan says so.
    let t = two_of_three(Network::Testnet);
    let v: Value = serde_json::from_str(
        &t.export(ExportFormat::Caravan, Network::Testnet)
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(v["network"], "testnet");
}

#[test]
fn names_and_filenames_are_sanitised() {
    assert_eq!(
        coldcard_name(Some("My: very #long wallet name here")),
        "My very long wallet"
    );
    assert_eq!(coldcard_name(Some("  ")), "Multisig");
    assert_eq!(coldcard_name(None), "Multisig");
    assert_eq!(coldcard_name(Some("Café ☕ fund")), "Caf fund");
    assert_eq!(file_stem(Some("Family Vault / 2024!")), "Family-Vault-2024");
    assert_eq!(file_stem(Some("../../etc/passwd")), "etc-passwd");
    assert_eq!(file_stem(Some("☕☕")), "multisig");
    assert_eq!(file_stem(None), "multisig");
}

// ---------------------------------------------------------------------------
// BIP-129 test vectors.

const BIP129_2OF2: &str = "BSMS 1.0
wsh(sortedmulti(2,[1cf0bf7e/48'/0'/0'/2']xpub6FL8FhxNNUVnG64YurPd16AfGyvFLhh7S2uSsDqR3Qfcm6o9jtcMYwh6DvmcBF9qozxNQmTCVvWtxLpKTnhVLN3Pgnu2D3pAoXYFgVyd8Yz/**,[4fc1dd4a/48'/0'/0'/2']xpub6EebMbEps7ZcV3FYEnddRsvrFWDrt2tiPmCeM7pPXQEmphvq9ZfJ1LWFUDjf3vxCeBuPrfyGrMazWUsYsetrnHatQZVLJH7LsgCjtMqdzgj/**))
/0/*,/1/*
bc1qrgc6p3kylfztu06ysl752gwwuekhvtfh9vr7zg43jvu60mutamcsv948ej";

const BIP129_STANDARD: &str = "BSMS 1.0
wsh(sortedmulti(2,[b7868815/48'/0'/0'/2']xpub6FA5rfxJc94K1kNtxRby1hoHwi7YDyTWwx1KUR3FwskaF6HzCbZMz3zQwGnCqdiFeMTPV3YneTGS2YQPiuNYsSvtggWWMQpEJD4jXU7ZzEh/**,[eedff89a/48'/0'/0'/2']xpub6EhJvMneoLWAf8cuyLBLQiKiwh89RAmqXEqYeFuaCEHdHwxSRfzLrUxKXEBap7nZSHAYP7Jfq6gZmucotNzpMQ9Sb1nTqerqW8hrtmx6Y6o/**))
/0/*,/1/*
bc1qhs4u273g4azq7kqqpe6vh5wfhasfmrq7nheyzsnq77humd7rwtkqagvakf";

const BIP129_NESTED_MULTI: &str = "BSMS 1.0
sh(wsh(multi(2,[793cc70b/48'/0'/0'/1']xpub6ErVmcYYHmavsMgxEcTZyzN5sqth1ZyRpFNJC26ij1wYGC2SBKYrgt9yariSbn7HLRoZUvhUhmPfsRTPrdhhGFscpPZzmch6UTdmRP1aZUj/**,[b3118e52/48'/0'/0'/1']xpub6Du5Jn6eYZE96ccmAc1ZTFPzdnzrvqfG4mpamDun2qZYKywoiQJMCbS3kWWMr6U3XW6s125RLsaPABWgv2yA749ieaMe67FxkTjMsbcxCch/**,[842bd2ed/48'/0'/0'/1']xpub6Ex81KopPkEt9hJiWHabYy8LNsSR4A7sUQoFBk9dR8XxHrr4p9HrYWN3NCf5uwfopHnQkCG7FYnZMztKbtRtbh6tzZC4xtHPbmVVxRSN7ic/**)))
/0/*,/1/*
3GzMtFXahiu4TpGNGFc4bHMvAcvz5vVQrT";

const BIP129_PUBKEYS: &str = "BSMS 1.0
wsh(sortedmulti(1,[59865f44/48'/0'/0'/2']026d15412460ba0d881c21837bb999233896085a9ed4e5445bd637c10e579768ba,[b7044ca6/48'/0'/0'/2']030baf0497ab406ff50cb48b4013abac8a0338758d2fd54cd934927afa57cc2062))#rzx9dffd
No path restrictions
bc1quqy523xu3l8che3s8vja8n33qtg0uyugr9l5z092s3wa50p8t7rqy6zumf";

const BIP129_KEY_RECORD: &str = "BSMS 1.0
00
[1cf0bf7e/48'/0'/0'/2']xpub6FL8FhxNNUVnG64YurPd16AfGyvFLhh7S2uSsDqR3Qfcm6o9jtcMYwh6DvmcBF9qozxNQmTCVvWtxLpKTnhVLN3Pgnu2D3pAoXYFgVyd8Yz
Signer 1 key
IB7v+qi1b+Xrwm/3bF+Rjl8QbIJ/FMQ40kUsOOQo1SqUWn5QlFWbBD8BKPRetfo1L1N7DmYjVscZNsmMrqRJGWw=";

#[test]
fn bip129_descriptor_records_import_and_export_byte_for_byte() {
    let n = Network::Bitcoin;
    for (record, sorted, script) in [
        (BIP129_2OF2, true, ScriptType::P2wsh),
        (BIP129_STANDARD, true, ScriptType::P2wsh),
        (BIP129_NESTED_MULTI, false, ScriptType::P2shP2wsh),
    ] {
        let c = MultisigConfig::import(record, n).unwrap();
        assert_eq!(c.script, script);
        assert_eq!(
            c.policy,
            Policy::Multi {
                threshold: 2,
                sorted
            }
        );
        assert_eq!(c.first_address(n).unwrap(), record.lines().last().unwrap());
        // Our export is the spec's record.
        let out = c.export(ExportFormat::Bsms, n).unwrap().content;
        assert_eq!(out.trim_end(), record);
    }
}

#[test]
fn bsms_import_checks_the_first_address_and_restrictions() {
    let n = Network::Bitcoin;
    // Tampered address.
    let bad = BIP129_2OF2.replace(
        "bc1qrgc6p3kylfztu06ysl752gwwuekhvtfh9vr7zg43jvu60mutamcsv948ej",
        "bc1qhs4u273g4azq7kqqpe6vh5wfhasfmrq7nheyzsnq77humd7rwtkqagvakf",
    );
    let e = MultisigConfig::import(&bad, n).unwrap_err().to_string();
    assert!(e.contains("doesn't match its keys"), "{e}");
    // Unsupported restrictions, missing lines, other versions.
    assert!(MultisigConfig::import(&BIP129_2OF2.replace("/0/*,/1/*", "/0/*"), n).is_err());
    assert!(MultisigConfig::import("BSMS 1.0\nwsh(sortedmulti(2,x))", n).is_err());
    assert!(MultisigConfig::import(&BIP129_2OF2.replace("BSMS 1.0", "BSMS 2.0"), n).is_err());
    // Fixed public keys only have one address: refused clearly.
    let e = MultisigConfig::import(BIP129_PUBKEYS, n)
        .unwrap_err()
        .to_string();
    assert!(e.contains("fixed public keys"), "{e}");
    // A round-1 key record is a cosigner, not a wallet...
    let e = MultisigConfig::import(BIP129_KEY_RECORD, n)
        .unwrap_err()
        .to_string();
    assert_eq!(e, COSIGNER_NOT_WALLET);
    // ...and imports as one.
    let k = cosigner_from_export(BIP129_KEY_RECORD, n).unwrap();
    assert_eq!(k.fingerprint, "1cf0bf7e");
    assert_eq!(k.path, "m/48'/0'/0'/2'");
    assert_eq!(k.label.as_deref(), Some("Signer 1 key"));
    // Our own BSMS read on the wrong network.
    let ours = two_of_three(n)
        .export(ExportFormat::Bsms, n)
        .unwrap()
        .content;
    assert!(matches!(
        MultisigConfig::import(&ours, Network::Testnet),
        Err(Error::Network(_))
    ));
}

/// Nunchuk's BSMS export (BlueWallet fixture): three-level paths, `/**`.
const NUNCHUK_BSMS: &str = "BSMS 1.0
wsh(sortedmulti(2,[3f5e4e09/48'/0'/0']xpub6BtEVX9WezWA2EYa8Wf1rJTokXpF96Rmqr4JKoR3SbjGw1vCa3Qb91mKN1AqJnBzSpqy44tEMB27Khne6KBmNzurXigfis7F28X5dRLWGoY/**,[b96bc7c7/48'/0'/0']xpub6CzCTuwFmAcBoxsfPLgiKfkJ2W3GbW1zxFKvCamZQ1J2XMobJR1gMNqP64pcFsy65ST4WcZtrCn99WXgLcWM6bqw6iFneAPTgJuLHggHD5S/**,[44437f41/48'/0'/0']xpub6Cn4cYgRhXKnvNjE4ULP2mzWHFw4WtCyp1nBuE97SyZVTPwq5xWkKsTVe9c16XSTr6V9AKMDgizav7z69j4AREaMKoFzUDbdcZhVq5xaKMU/**))
/0/*,/1/*
bc1qjppd555998lvpgdew30kdrvu2qg90n5gg684khrc2u5g7kj8cnuqujvz3p
";

#[test]
fn nunchuk_bsms_and_descriptor() {
    let n = Network::Bitcoin;
    let c = MultisigConfig::import(NUNCHUK_BSMS, n).unwrap();
    let fps: Vec<&str> = c.cosigners.iter().map(|k| k.fingerprint.as_str()).collect();
    assert_eq!(fps, ["3f5e4e09", "b96bc7c7", "44437f41"]);
    assert_eq!(c.cosigners[0].path, "m/48'/0'/0'");
    assert_eq!(
        c.first_address(n).unwrap(),
        "bc1qjppd555998lvpgdew30kdrvu2qg90n5gg684khrc2u5g7kj8cnuqujvz3p"
    );
    // Nunchuk's own descriptor export writes `xpub/*`, meaning /0/* and /1/*.
    let keys = c
        .cosigners
        .iter()
        .map(|k| format!("{}/*", origin_apostrophe(k)))
        .collect::<Vec<_>>()
        .join(",");
    let nunchuk = with_checksum(&format!("wsh(sortedmulti(2,{keys}))")).unwrap();
    assert_eq!(MultisigConfig::import(&nunchuk, n).unwrap(), c);
    // And in its JSON wrapper, with a name.
    let json = format!(r#"{{"descriptor":"{nunchuk}","label":"Nunchuk wallet"}}"#);
    let got = MultisigConfig::import(&json, n).unwrap();
    assert_eq!(got.name.as_deref(), Some("Nunchuk wallet"));
    assert_eq!(without_names(got), c);
}

// ---------------------------------------------------------------------------
// Coldcard-style text files.

/// BlueWallet fixture: CoboVault file with per-key `# derivation:` lines.
const COBO_CUSTOM_PATHS: &str = "# CoboVault Multisig setup file (created on D37EAD88)
#
Name: CV_33B5B91A_2-2
Policy: 2 of 2
Format: P2WSH

# derivation: m/47'/0'/0'/1'
D37EAD88: Zpub74ijpfhERJNjhCKXRspTdLJV5eoEmSRZdHqDvp9kVtdVEyiXk7pXxRbfZzQvsDFpfDHEHVtVpx4Dz9DGUWGn2Xk5zG5u45QTMsYS2vjohNQ

# derivation: m/46'/0'/0'/1'
168DD603: Zpub75mAE8EjyxSzoyPmGnd5E6MyD7ALGNndruWv52xpzimZQKukwvEfXTHqmH8nbbc6ccP5t2aM3mws3pKYSnKpKMMytdbNEZFUxKzztYFM8Pn
";

/// BlueWallet fixture: Caravan (unchained-wallets) P2SH file with a
/// `Derivation:` line before each key.
const CARAVAN_COLDCARD_P2SH: &str = "# Coldcard Multisig setup file (exported from unchained-wallets)
# https://github.com/unchained-capital/unchained-wallets
# v1.0.0
#
Name: yIm388Ib_x3g6ruuY
Policy: 2 of 3
Format: P2SH

Derivation: m/0/0/0/0
0dfd4a3b: xpub6EDykLBC5EbV78g9QiFKZtsPnesv2m4Xtbvzi8FjwzGmBJbKVGRUDKcnpf9QyFQrv3vh6C9N92rKaqURHmhhs8EigCTZY7qAsFDGg4QeaU7
Derivation: m/45'/0/0/0
8887a204: xpub6Dma5xev7J5QJuVQ7tGRWGY56FNkt9qX5gCdFQe8fHp4DmtBuxUpxGMo1oBhKYi7fYxqHa2UpPR87Xdc6i7AeZt3tdFRykiqnRk7vKyDcbY
Derivation: m/45'/0/0/0
77c5100e: xpub6DqutttHLMzHBHk6LFJnVkv2EydG6LqN5GAFLFyCFix446y8iMqvavic9zTvgS3kynpD5ycLNKiaKx9c96qvL5VXHqfNZEuGJstJ5tpFfwL
";

/// BlueWallet fixture: Sparrow's Coldcard export.
const SPARROW_COLDCARD: &str = "# Coldcard Multisig setup file (created by Sparrow)
#
Name: name
Policy: 2 of 3
Derivation: m/48'/0'/0'/2'
Format: P2WSH

A3909080: xpub6EJoguJn6okK6ymxkU53BZgjf4kqrL19uvTwqVnPRaV8qYvx1RhGtyKZxLhzY4YLCYtvB6DsGk1uR3GFBMvwgc5qz19WAxm2nSXZcoyNftY
7AB71DF0: xpub6FL52qF9cn2nL7T8gv6gStR8GcezKF4LgY53VLbi5sqRjc4PpBuYFp3nfep884hkuF9dC7KZ11mkthE1NVzbX98QmoJQqBFxQbHnpJ5vQKB
F11B9FF2: xpub6ERjK6ubp5p7UUByXGadKvHcqEUMxvWPr7DkzqHz7GnnZEqdR9ffE3Y1DffYqRjJzeGWL1st1qH4kEaZCzjVnJoZ2PJUcnThi48D71zd6sJ
";
const SPARROW_FIRST: &str = "bc1qtysquqsjqjfqvhd6l2h470hdgwhcahs4nq2ca49cyxftwjnjt9ssh8emel";

#[test]
fn coldcard_text_files_from_other_apps() {
    let n = Network::Bitcoin;
    let c = MultisigConfig::import(COBO_CUSTOM_PATHS, n).unwrap();
    assert_eq!(c.name.as_deref(), Some("CV_33B5B91A_2-2"));
    assert_eq!(c.cosigners[0].fingerprint, "d37ead88");
    assert_eq!(c.cosigners[0].path, "m/47'/0'/0'/1'");
    assert_eq!(c.cosigners[1].path, "m/46'/0'/0'/1'");
    assert!(c.cosigners[0].xpub.starts_with("xpub"));
    assert_eq!(
        receive_address(&c, n, 0),
        "bc1qxzrzh4caw7e3genwtldtxntzj0ktfl7mhf2lh4fj8h7hnkvtvc4salvp85"
    );
    assert_eq!(
        receive_address(&c, n, 1),
        "bc1qvwd2d7r46j7u9qyxpedfhe5p075sxuhzd0n6napuvvhq2u5nrmqs9ex90q"
    );
    assert_eq!(
        change_address(&c, n, 0),
        "bc1qtah0p50d4qlftn049k7lldcwh7cs3zkjy9g8xegv63p308hsh9zsf5567q"
    );
    // Exported back with per-key paths, it reads the same.
    let again = MultisigConfig::import(&c.export(ExportFormat::Coldcard, n).unwrap().content, n);
    assert_eq!(again.unwrap(), c);

    let p2sh = MultisigConfig::import(CARAVAN_COLDCARD_P2SH, n).unwrap();
    assert_eq!(p2sh.script, ScriptType::P2sh);
    assert_eq!(p2sh.cosigners[0].path, "m/0/0/0/0");
    assert_eq!(p2sh.cosigners[1].path, "m/45'/0/0/0");
    assert_eq!(
        p2sh.first_address(n).unwrap(),
        "3KEDMaBiSEmqvkjHgsFZt73MsArxK2LKKq"
    );

    let sparrow = MultisigConfig::import(SPARROW_COLDCARD, n).unwrap();
    assert_eq!(sparrow.first_address(n).unwrap(), SPARROW_FIRST);
    assert_eq!(sparrow.name.as_deref(), Some("name"));
}

#[test]
fn coldcard_text_quirks() {
    let n = Network::Bitcoin;
    let c = two_of_three(n);
    let base = c.export(ExportFormat::Coldcard, n).unwrap().content;
    // Lowercase hex fingerprints, `h` paths, CRLF, SLIP-132 Zpubs, `2/3`.
    let zpub_file = base
        .replace("\n", "\r\n")
        .replace("m/48'/0'/0'/2'", "m/48h/0h/0h/2h")
        .replace("Policy: 2 of 3", "Policy: 2/3")
        .replace(&c.cosigners[0].fingerprint.to_uppercase(), &c.cosigners[0].fingerprint)
        .replace(
            &c.cosigners[0].xpub,
            "Zpub74Jru6aftwwHxCUCWEvP6DgrfFsdA4U6ZRtQ5i8qJpMcC39yZGv3egBhQfV3MS9pZtH5z8iV5qWkJsK6ESs6mSzt4qvGhzJxPeeVS2e1zUG",
        );
    let got = MultisigConfig::import(&zpub_file, n).unwrap();
    assert_eq!(got, expected_after(ExportFormat::Coldcard, &c));
    // Nested and legacy spellings.
    let nested = base.replace("Format: P2WSH", "Format: P2SH-P2WSH");
    assert_eq!(
        MultisigConfig::import(&nested, n).unwrap().script,
        ScriptType::P2shP2wsh
    );
    let nested = base.replace("Format: P2WSH", "Format: p2wsh-p2sh");
    assert_eq!(
        MultisigConfig::import(&nested, n).unwrap().script,
        ScriptType::P2shP2wsh
    );
    // No Policy line: N of N, as Coldcard does.
    let all = base.replace("Policy: 2 of 3\n", "");
    assert_eq!(MultisigConfig::import(&all, n).unwrap().threshold(), 3);
    // Wrong count, missing derivation, seeds, Taproot.
    let e = MultisigConfig::import(&base.replace("2 of 3", "2 of 4"), n).unwrap_err();
    assert!(e.to_string().contains("2 of 4, but lists 3 keys"), "{e}");
    let e = MultisigConfig::import(&base.replace("Derivation: m/48'/0'/0'/2'\n", ""), n);
    assert!(e.unwrap_err().to_string().contains("derivation path"));
    let seeds = base.replace("Format: P2WSH\n", "Format: P2WSH\nseed: abandon abandon\n");
    assert_eq!(
        MultisigConfig::import(&seeds, n).unwrap_err().to_string(),
        SEED_IN_FILE
    );
    assert!(MultisigConfig::import(&base.replace("P2WSH", "P2TR"), n).is_err());
    // A path that doesn't match the key's depth is caught.
    let e = MultisigConfig::import(&base.replace("m/48'/0'/0'/2'", "m/48'/0'/0'"), n)
        .unwrap_err()
        .to_string();
    assert!(e.contains("doesn't match its xpub"), "{e}");
    // Mainnet file on testnet.
    assert!(matches!(
        MultisigConfig::import(&base, Network::Testnet),
        Err(Error::Network(_))
    ));
}

// ---------------------------------------------------------------------------
// Caravan.

/// Caravan repo fixture `bitcoin-2-of-2-P2WSH_MAINNET.json`.
const CARAVAN_P2WSH_M: &str = r#"{
  "name": "P2WSH-M",
  "addressType": "P2WSH",
  "network": "mainnet",
  "client":  {
    "type": "public"
  },
  "quorum": {
    "requiredSigners": 2,
    "totalSigners": 2
  },
  "extendedPublicKeys": [
    {
        "name": "six",
        "bip32Path": "m/48'/0'/100'/2'",
        "xpub": "xpub6EwJjKaiocGvqSuM2jRZSuQ9HEddiFUFu9RdjE47zG7kXVNDQpJ3GyvskwYiLmvU4SBTNZyv8UH53QcmFEE23YwozE61V3dwzZJEFQr6H2b",
        "xfp" : "00000006"
      },
    {
        "name": "osw",
        "bip32Path": "m/48'/0'/100'/2'",
        "xpub": "xpub6DcqYQxnbefzFkaRBK63FSE2GzNuNnNhFGw1xV9RioVG7av6r3JDf1aELqBSq5gt5487CtNxvVtaiJjQU2HQWzgG5NzLyTPbYav6otW8qEc",
        "xfp" : "f57ec65d"
      }
  ],
  "startingAddressIndex": 0
}"#;

/// Caravan repo fixture `coldcard/wc-p2wsh_m.txt` (same wallet, keys reversed).
const CARAVAN_WC_P2WSH_M: &str = "# Coldcard Multisig setup file for test suite

Name: p2wsh-m
Policy: 2 of 2
Format: P2WSH
Derivation: m/48'/0'/100'/2'

f57ec65d: xpub6DcqYQxnbefzFkaRBK63FSE2GzNuNnNhFGw1xV9RioVG7av6r3JDf1aELqBSq5gt5487CtNxvVtaiJjQU2HQWzgG5NzLyTPbYav6otW8qEc
00000006: xpub6EwJjKaiocGvqSuM2jRZSuQ9HEddiFUFu9RdjE47zG7kXVNDQpJ3GyvskwYiLmvU4SBTNZyv8UH53QcmFEE23YwozE61V3dwzZJEFQr6H2b
";

/// Caravan repo fixtures, P2SH-P2WSH testnet.
const CARAVAN_NESTED_T: &str = r#"{
  "name": "P2SH-P2WSH-T",
  "addressType": "P2SH-P2WSH",
  "network": "testnet",
  "client":  { "type": "public" },
  "quorum": { "requiredSigners": 2, "totalSigners": 2 },
  "extendedPublicKeys": [
    { "name": "osw", "bip32Path": "m/48'/1'/100'/1'",
      "xpub": "tpubDFc9Mm4tw6EkdXuk24MnQYRrDsdKEFh498vFffqa2KJmxytpcHbWrcFYwTKAdLxkSWpadzb5M5VVZ7PDAUjDjymvUmQ7pBbRecz2FM952Am",
      "xfp" : "f57ec65d" },
    { "name": "two", "bip32Path": "m/48'/1'/100'/1'",
      "xpub": "tpubDErWN5qfdLwY9ZJo9HWpxjcuEFuEBVHSbQbPqF35LQr3etWNGirKcgAa93DZ4DmtHm36p2gTf4aj6KybLqHaS3UePM5LtPqtb3d3dYVDs2F",
      "xfp" : "00000002" }
  ],
  "startingAddressIndex": 0
}"#;
const CARAVAN_WC_NESTED_T: &str = "# Coldcard Multisig setup file for test suite

Name: p2sh-p2wsh-t
Policy: 2 of 2
Format: P2WSH-P2SH
Derivation: m/48'/1'/100'/1'

f57ec65d: tpubDFc9Mm4tw6EkdXuk24MnQYRrDsdKEFh498vFffqa2KJmxytpcHbWrcFYwTKAdLxkSWpadzb5M5VVZ7PDAUjDjymvUmQ7pBbRecz2FM952Am
00000002: tpubDErWN5qfdLwY9ZJo9HWpxjcuEFuEBVHSbQbPqF35LQr3etWNGirKcgAa93DZ4DmtHm36p2gTf4aj6KybLqHaS3UePM5LtPqtb3d3dYVDs2F
";

/// Caravan repo fixtures, P2SH testnet (BIP45-style paths).
const CARAVAN_P2SH_T: &str = r#"{
  "name": "P2SH-T",
  "addressType": "P2SH",
  "network": "testnet",
  "client":  { "type": "public" },
  "quorum": { "requiredSigners": 2, "totalSigners": 2 },
  "extendedPublicKeys": [
    { "name": "one", "bip32Path": "m/45'/1'/100'",
      "xpub": "tpubDDinbKDXyddTUKcX6mv936Ux5utCJteq5S6EEKhfpM8CqN2rMAcccv6GecsB3cPt8eGL4e4K2eaZ9Jis9TGf7mbwBsRTN7ngnFR7yJZxBKC",
      "xfp" : "00000001" },
    { "name": "osw", "bip32Path": "m/45'/1'/100'",
      "xpub": "tpubDDQubdBx9cbwQtdcRTisKF7wVCwHgHewhU7wh77VzCi62Q9q81qyQeLoZjKWZ62FnQbWU8k7CuKo2A21pAWaFtPGDHP9WuhtAx4smcCxqn1",
      "xfp" : "f57ec65d" }
  ],
  "startingAddressIndex": 0
}"#;
const CARAVAN_WC_P2SH_T: &str = "# Coldcard Multisig setup file for test suite

Name: p2sh-t
Policy: 2 of 2
Format: P2SH
Derivation: m/45'/1'/100'

f57ec65d: tpubDDQubdBx9cbwQtdcRTisKF7wVCwHgHewhU7wh77VzCi62Q9q81qyQeLoZjKWZ62FnQbWU8k7CuKo2A21pAWaFtPGDHP9WuhtAx4smcCxqn1
00000001: tpubDDinbKDXyddTUKcX6mv936Ux5utCJteq5S6EEKhfpM8CqN2rMAcccv6GecsB3cPt8eGL4e4K2eaZ9Jis9TGf7mbwBsRTN7ngnFR7yJZxBKC
";

fn same_wallet(a: &MultisigConfig, b: &MultisigConfig, network: Network) {
    assert_eq!(a.script, b.script);
    assert_eq!(a.policy, b.policy);
    for i in 0..3 {
        assert_eq!(
            receive_address(a, network, i),
            receive_address(b, network, i)
        );
        assert_eq!(change_address(a, network, i), change_address(b, network, i));
    }
}

#[test]
fn caravan_fixtures_match_their_coldcard_files() {
    for (json_file, txt, network, script) in [
        (
            CARAVAN_P2WSH_M,
            CARAVAN_WC_P2WSH_M,
            Network::Bitcoin,
            ScriptType::P2wsh,
        ),
        (
            CARAVAN_NESTED_T,
            CARAVAN_WC_NESTED_T,
            Network::Testnet,
            ScriptType::P2shP2wsh,
        ),
        (
            CARAVAN_P2SH_T,
            CARAVAN_WC_P2SH_T,
            Network::Testnet,
            ScriptType::P2sh,
        ),
    ] {
        let a = MultisigConfig::import(json_file, network).unwrap();
        let b = MultisigConfig::import(txt, network).unwrap();
        assert_eq!(a.script, script);
        same_wallet(&a, &b, network);
        // Names come along.
        assert!(a.name.is_some() && b.name.is_some());
        assert!(a.cosigners.iter().all(|k| k.label.is_some()));
        // Signet reads testnet files; mainnet doesn't.
        if network == Network::Testnet {
            assert!(MultisigConfig::import(json_file, Network::Signet).is_ok());
            assert!(matches!(
                MultisigConfig::import(json_file, Network::Bitcoin),
                Err(Error::Network(_))
            ));
        }
    }
    let m = MultisigConfig::import(CARAVAN_P2WSH_M, Network::Bitcoin).unwrap();
    assert_eq!(m.name.as_deref(), Some("P2WSH-M"));
    assert_eq!(m.cosigners[0].label.as_deref(), Some("six"));
    assert_eq!(m.cosigners[0].fingerprint, "00000006");
    assert!(matches!(
        MultisigConfig::import(CARAVAN_P2WSH_M, Network::Testnet),
        Err(Error::Network(_))
    ));
}

/// BlueWallet fixture: Caravan file with an "Unknown" path and no client.
const BLUEWALLET_CARAVAN: &str = r#"{
  "name": "Vault #1",
  "addressType": "P2WSH",
  "network": "mainnet",
  "quorum": { "requiredSigners": 2, "totalSigners": 3 },
  "startingAddressIndex": 0,
  "extendedPublicKeys": [
    { "name": "io", "xpub": "xpub6EJoguJn6okK6ymxkU53BZgjf4kqrL19uvTwqVnPRaV8qYvx1RhGtyKZxLhzY4YLCYtvB6DsGk1uR3GFBMvwgc5qz19WAxm2nSXZcoyNftY", "bip32Path": "m/48'/0'/0'/2'", "xfp": "A3909080" },
    { "name": "bluewater", "xpub": "xpub6FL52qF9cn2nL7T8gv6gStR8GcezKF4LgY53VLbi5sqRjc4PpBuYFp3nfep884hkuF9dC7KZ11mkthE1NVzbX98QmoJQqBFxQbHnpJ5vQKB", "bip32Path": "m/48'/0'/0'/2'", "xfp": "7AB71DF0" },
    { "name": "Unknown", "xpub": "xpub6ERjK6ubp5p7UUByXGadKvHcqEUMxvWPr7DkzqHz7GnnZEqdR9ffE3Y1DffYqRjJzeGWL1st1qH4kEaZCzjVnJoZ2PJUcnThi48D71zd6sJ", "bip32Path": "Unknown", "xfp": "F11B9FF2" }
  ]
}"#;

/// BlueWallet test: Caravan "text" keys with no fingerprint or path.
const CARAVAN_NO_ORIGINS: &str = r#"{"name":"My Multisig Wallet","addressType":"P2WSH","network":"mainnet","client":{"type":"public"},"quorum":{"requiredSigners":2,"totalSigners":3},"extendedPublicKeys":[{"name":"Extended Public Key 1","bip32Path":"Unknown (make sure you have written this down previously!)","xpub":"xpub6EA866cxYyjQa2mupVnEP5mg1vU5fqkyUo97Sm6SN73KWbXAUQ78dBRTisYHJxj5cTyduxhG2Qxd6QNNjtHoHaGDR7aeUrJUvh9GfqvsRQQ","method":"text"},{"name":"Extended Public Key 2","bip32Path":"Unknown (make sure you have written this down previously!)","xpub":"xpub6FCYVZAU7dofgor9fQaqyqqA9NqBAn83iQpoayuWrwBPfwiPgCXGCD7dvAG93M5MZs5VWVP7FErGA5UeiALqaPt7KV67fL9WX9bqXTyeWxb","method":"text"},{"name":"Extended Public Key 3","bip32Path":"Unknown (make sure you have written this down previously!)","xpub":"xpub6EBRM9zwt7Wmkvte61c4fshZ7ZJDaZiaC27WxTkCq5hdNYPodJY4wayCvMNH4ysF944HaBoS4dVrcfhaHwowTn9TJ7EPWE8hJAZjv7gwtew","method":"text"}]}"#;

#[test]
fn caravan_files_from_bluewallet() {
    let n = Network::Bitcoin;
    let c = MultisigConfig::import(BLUEWALLET_CARAVAN, n).unwrap();
    assert_eq!(c.first_address(n).unwrap(), SPARROW_FIRST);
    assert_eq!(c.cosigners[0].fingerprint, "a3909080");
    assert_eq!(c.cosigners[2].path, "m");
    assert_eq!(c.cosigners[1].label.as_deref(), Some("bluewater"));

    let c = MultisigConfig::import(CARAVAN_NO_ORIGINS, n).unwrap();
    assert!(c
        .cosigners
        .iter()
        .all(|k| k.fingerprint == UNKNOWN_FINGERPRINT && k.path == "m"));
    assert_eq!(
        receive_address(&c, n, 0),
        "bc1qnpy7c7wz6tvmhdwgyk8ka4du3s9x6uhgjal305xdatmwfa538zxsys5l0t"
    );
    assert_eq!(
        change_address(&c, n, 1),
        "bc1qpqa9c6nkqgcruegnh8wcsr0gzc4x9y90v9k0nxr6lww0gts430zqp7wm86"
    );
}

// ---------------------------------------------------------------------------
// Specter, Sparrow, Electrum, Casa: JSON and descriptors.

const FULLYNODED_2OF3: &str = r#"{"label":"Multisig","blockheight":649459,"descriptor":"wsh(sortedmulti(2,[1104442d/48h/0h/0h/2h]xpub6ERaLLFZ3qu7X4cpiMAvSZ6UZVXJfxY5FoNvVJgai1V78DmeNHTcNVfu4cK2RmvTNXU4s1tFpGMPTwqoQ1RraE2o9iiNw2s2aHESpandSFY/0/*,[8cce63f8/48h/0h/0h/2h]xpub6FCSLcRY99737oUAnvXd1k2gSz9P4zi4gQJ8UChSPSCxCK7XS9kLzoLHKNBiR26d3ivT7w3oka9f4BepVLoQ875XzgejjbDo626R6NBUJDW/0/*,[bf27bd7b/48h/0h/0h/2h]xpub6FE9uTPh1RxPRAfFVaET75vdfdQzXKZrT7LxukkqY4KhwUm4haMSPCwERfPouG6da6uZTRCXettvYFDck7nbw6JdBztGr1VBLonWch7NpJo/0/*))#erxvm6x2"}"#;

const SPECTER_NESTED: &str = "sh(wsh(sortedmulti(2,[99fe7770/48h/0h/0h/1h]xpub6FEEbEaYM9pmY8rxz4g6AuaJVszwKt8g6cFg9nFWeE85EdBrGBcnhHqXAaPbQ4Hi3Xu9vijtYdYnNjERw9eSniF3235Vjde11GieeHjv7XT/0/*,[636bdad0/48h/0h/0h/1h]xpub6F67TyyWngU5rkVPxHTdmuYkaXHXeRwwVg5SsDeiPPjt6Mithh4Qzpu2yHjNa5W7nhcTbV6QaJMvppYMDSnB3SxArCkp9GvHQqpr5P17yFv/0/*,[99c90b2f/48h/0h/0h/1h]xpub6E6FyTrwmuUYeRMULXSAGvUKeP5ba6pQKVhWNuvVZFmGPnDYb9m5vP2XsSEQ4gKUGfXtLcKs4AV31vpfx2P5KuWm9co4HM3FtGov8enmJ6f/0/*)))#wy7xtlnw";

/// Sparrow's Specter export: account keys with no branch at all.
const SPARROW_SPECTER: &str = r#"{
  "label": "name",
  "blockheight": 655372,
  "descriptor": "wsh(sortedmulti(2,[a3909080/48'/0'/0'/2']xpub6EJoguJn6okK6ymxkU53BZgjf4kqrL19uvTwqVnPRaV8qYvx1RhGtyKZxLhzY4YLCYtvB6DsGk1uR3GFBMvwgc5qz19WAxm2nSXZcoyNftY,[7ab71df0/48'/0'/0'/2']xpub6FL52qF9cn2nL7T8gv6gStR8GcezKF4LgY53VLbi5sqRjc4PpBuYFp3nfep884hkuF9dC7KZ11mkthE1NVzbX98QmoJQqBFxQbHnpJ5vQKB,[f11b9ff2/48'/0'/0'/2']xpub6ERjK6ubp5p7UUByXGadKvHcqEUMxvWPr7DkzqHz7GnnZEqdR9ffE3Y1DffYqRjJzeGWL1st1qH4kEaZCzjVnJoZ2PJUcnThi48D71zd6sJ))#8xvx6q6r"
}"#;

/// Sparrow's Electrum export (no fingerprints; one keystore has an xprv,
/// which is ignored).
const SPARROW_ELECTRUM: &str = r#"{
  "x1/": { "xpub": "Zpub74sRRUP3y8PeE9KaMr7GRpDYio5zwvfk4RA4KYqhZN5JZw8KH9QgEDVmoTae6K55FJCWYdR75H7WJn78uyuvRZZiZ9ekkBs1DcviysT968p", "type": "bip32", "pw_hash_version": 1 },
  "x2/": { "xpub": "Zpub75tgmQKRV6g7TGzkJJ8uh8wwLLz9Qqivq2m9yPf2DfRbTzFm5ucwb4DzWmgmgKEVwzTDZeWnoYsMnS4u77yaG6cHLwofQQMvqmgxBKiWEav", "xprv": "ZprvAjNnvkiSoWRpoiQR4Jkr5TwqbRVtCUZHCDKwfZkjyaSzsY9NPEHKtXb7fCyoQn8nqNgvjasBQMSMooMM9KegJKc1kAsr2asWgpnYkRYFU1o", "type": "bip32", "pw_hash_version": 1 },
  "x3/": { "xpub": "Zpub74zM3fysgQTSbdjb8ecraApRtxoX4XAyzbusUtMJF4NxHd2zgsP4ZHiD4nYCPgG43Pa6hZ57pNNfdyRSwciUXGHRbXojC1Zg9EXNU3hjVG1", "type": "bip32", "pw_hash_version": 1 },
  "wallet_type": "2of3",
  "use_encryption": false,
  "seed_type": "bip39"
}"#;

/// BlueWallet fixture: Coldcard's Electrum export (`ckcc_xfp`).
const COLDCARD_ELECTRUM: &str = r#"{"x2/": {"xpub": "Zpub75mAE8EjyxSzoyPmGnd5E6MyD7ALGNndruWv52xpzimZQKukwvEfXTHqmH8nbbc6ccP5t2aM3mws3pKYSnKpKMMytdbNEZFUxKzztYFM8Pn", "hw_type": "coldcard", "ckcc_xfp": 64392470, "label": "Coldcard", "derivation": "m/48'/1'/0'/1'", "type": "hardware"}, "x1/": {"xpub": "Zpub74ijpfhERJNjhCKXRspTdLJV5eoEmSRZdHqDvp9kVtdVEyiXk7pXxRbfZzQvsDFpfDHEHVtVpx4Dz9DGUWGn2Xk5zG5u45QTMsYS2vjohNQ", "hw_type": "coldcard", "ckcc_xfp": 2293071571, "label": "Coldcard", "derivation": "m/48'/1'/0'/1'", "type": "hardware"}, "wallet_type": "2of2", "use_encryption": false, "seed_version": 17}"#;

/// BlueWallet test: Casa 3-of-5 nested, origins without paths or with
/// unhardened ones, Ypub keys.
const CASA_3OF5: &str = "sh(wsh(sortedmulti(3,[35282ae3]Ypub6j9gr7f7uvWfTHdwVwhYRowZH9oqkh9DZz2hdbeKJw7yjQbsfqaruLva2piTZqN7jSKT43LpsnAqHU4vufUGAyGBzD6UJpwTeKiG5JRYUnm/<0;1>/*,[1b373e31/49/0/5]Ypub6iPAfqqinG5VHf2sJ3XfRjFh8BXm4C6x4U3DaPyu4RKocnqzb1peStw8Q624jwXpMZ5G81s7uwq8qLQ8rW55aZ3LvBbr9XVLTYxG78k28PE/<0;1>/*,[0d7bb846/49/0/5]Ypub6hwdyxEzktht47xgyyFY7TFrAzW7XgGbMSqS3QzfosYekG9LibhpohY6LrsjfP55VyU8i7iWS2s2Vs6RkAeze6bSpU3rNHsmESymE8X3J3k/<0;1>/*,[faa45c74/49/0/5]Ypub6iSiDkVrTTEh6eFbkCvohn1FfuhGr1Co9MCCHi6VPTT6crnX9Avq3PnZj7JcqhDTkWoDLEyeWmaPyxDY22b7k652Sg2eMc2g5tU6GDTvSng/<0;1>/*,[0551354e/49/0/5]Ypub6ipvk7JbDUXormRsSxXRf9eTUAVFwcjDfZXWYxRdN6nV7MNnnuD3WZBYjjWnMTncHhKJsnfkUGVYMRgQthZqY2wfHfMaJoVYBBhHT7MACGs/<0;1>/*)))";

#[test]
fn json_and_descriptor_files_from_other_apps() {
    let n = Network::Bitcoin;
    let c = MultisigConfig::import(FULLYNODED_2OF3, n).unwrap();
    assert_eq!(c.name.as_deref(), Some("Multisig"));
    assert_eq!(c.cosigners[0].path, "m/48'/0'/0'/2'");
    assert_eq!(
        c.first_address(n).unwrap(),
        "bc1q338rmdygx0weah4pdrp9xyycxlv2t48276gk3gxmg6m7xdkkglsqgzm6mz"
    );
    assert_eq!(
        change_address(&c, n, 0),
        "bc1qcgn73pjlwtt6krs2u6as0kh2jp486fa0t93yyq4d7xxxc37rf24qg67ewq"
    );

    let nested = MultisigConfig::import(SPECTER_NESTED, n).unwrap();
    assert_eq!(nested.script, ScriptType::P2shP2wsh);
    assert_eq!(
        nested.first_address(n).unwrap(),
        "3GSZaKT3LujScx6JeWejc6xjZsCDRzptsA"
    );
    assert_eq!(
        receive_address(&nested, n, 1),
        "3GT11kStn8W6q2kj257uZqW9xEKJwPMDkw"
    );

    for file in [SPARROW_SPECTER, SPARROW_ELECTRUM] {
        let c = MultisigConfig::import(file, n).unwrap();
        assert_eq!(c.first_address(n).unwrap(), SPARROW_FIRST, "{file}");
    }
    let electrum = MultisigConfig::import(SPARROW_ELECTRUM, n).unwrap();
    assert!(electrum
        .cosigners
        .iter()
        .all(|k| k.xpub.starts_with("xpub")));

    let cc = MultisigConfig::import(COLDCARD_ELECTRUM, n).unwrap();
    let fps: Vec<&str> = cc
        .cosigners
        .iter()
        .map(|k| k.fingerprint.as_str())
        .collect();
    assert_eq!(fps, ["d37ead88", "168dd603"]);
    assert_eq!(cc.cosigners[0].label.as_deref(), Some("Coldcard"));
    assert_eq!(
        cc.first_address(n).unwrap(),
        "bc1qxzrzh4caw7e3genwtldtxntzj0ktfl7mhf2lh4fj8h7hnkvtvc4salvp85"
    );

    let casa = MultisigConfig::import(CASA_3OF5, n).unwrap();
    assert_eq!(casa.threshold(), 3);
    assert_eq!(casa.total(), 5);
    assert_eq!(casa.cosigners[0].path, "m");
    assert_eq!(casa.cosigners[1].path, "m/49/0/5");
    assert_eq!(
        casa.first_address(n).unwrap(),
        "3HJiAohE25FFBLPLZGVDwv7ZbSXVsSiZH7"
    );
    assert_eq!(
        receive_address(&casa, n, 2),
        "3KrAqZwVwND5XFNEAnCuWyf4nPNAfF2JCF"
    );
    assert_eq!(
        change_address(&casa, n, 0),
        "36oNF12VNk4G5hPRR5zuZixnNjJikkTSWD"
    );
}

#[test]
fn descriptor_text_variants() {
    let n = Network::Bitcoin;
    let c = without_names(two_of_three(n));
    let multipath = c.descriptor_with_checksum().unwrap();
    let pair = c.descriptors().unwrap();
    // Without checksum, receive-only with checksum, `'` instead of `h`.
    for text in [
        multipath.split('#').next().unwrap().to_string(),
        with_checksum(&pair.external).unwrap(),
        pair.external.replace("h/", "'/").replace("h]", "']"),
        with_checksum(
            &multipath
                .split('#')
                .next()
                .unwrap()
                .replace("h/", "'/")
                .replace("h]", "']"),
        )
        .unwrap(),
    ] {
        assert_eq!(MultisigConfig::import(&text, n).unwrap(), c, "{text}");
    }
    // Sparrow's descriptor file: comments, then three descriptors.
    let sparrow = format!(
        "# Receive and change descriptor:\n{multipath}\n\n\n# Receive descriptor:\n{}\n\n# Change descriptor:\n{}\n",
        with_checksum(&pair.external).unwrap(),
        with_checksum(&pair.internal).unwrap()
    );
    assert_eq!(MultisigConfig::import(&sparrow, n).unwrap(), c);
    // Coldcard's "pretty" descriptor: one key per line, comments inside,
    // checksum of the compact form.
    let keys: Vec<String> = c
        .cosigners
        .iter()
        .map(|k| format!("{}/<0;1>/*", k.key_expression()))
        .collect();
    let sum = multipath.rsplit_once('#').unwrap().1;
    let pretty = format!(
        "# Coldcard descriptor export\n# order of keys in the descriptor does not matter\n# native segwit - p2wsh\nwsh(sortedmulti(\n\t# 2 of 3 (threshold)\n\t2,\n\t{},\n\t{},\n\t{}\n))#{sum}\n",
        keys[0], keys[1], keys[2]
    );
    assert_eq!(MultisigConfig::import(&pretty, n).unwrap(), c);
    // SLIP-132 keys inside a descriptor.
    let zpub = c.descriptors().unwrap().external.replace(
        &c.cosigners[0].xpub,
        "Zpub74Jru6aftwwHxCUCWEvP6DgrfFsdA4U6ZRtQ5i8qJpMcC39yZGv3egBhQfV3MS9pZtH5z8iV5qWkJsK6ESs6mSzt4qvGhzJxPeeVS2e1zUG",
    );
    assert_eq!(MultisigConfig::import(&zpub, n).unwrap(), c);
}

#[test]
fn import_errors_are_plain_english() {
    let n = Network::Bitcoin;
    let msg = |text: &str| MultisigConfig::import(text, n).unwrap_err().to_string();
    assert_eq!(msg("hello there"), NOT_MULTISIG);
    assert_eq!(msg("   "), "That file is empty");
    assert_eq!(msg(r#"{"foo": 1}"#), NOT_MULTISIG);
    assert!(msg("{not json").contains("isn't valid"));
    let master = Seed::from_phrase(ABANDON).unwrap().master_xpriv(n).unwrap();
    let single = format!(
        "wpkh({}/<0;1>/*)",
        key_expression(&master, "m/84'/0'/0'").unwrap()
    );
    assert_eq!(msg(&single), SINGLE_SIG_WALLET);
    assert_eq!(
        msg(&format!(r#"{{"label":"x","descriptor":"{single}"}}"#)),
        SINGLE_SIG_WALLET
    );
    assert_eq!(
        msg(r#"{"wallet_type":"standard","keystore":{}}"#),
        SINGLE_SIG_WALLET
    );
    // A cosigner export handed to the wallet importer.
    assert_eq!(msg(COLDCARD_MULTISIG_EXPORT), COSIGNER_NOT_WALLET);
    assert_eq!(
        msg(&key_expression(&master, "m/48'/0'/0'/2'").unwrap()),
        COSIGNER_NOT_WALLET
    );
    // Change-only descriptor; damaged checksum; Taproot.
    let c = two_of_three(n);
    let pair = c.descriptors().unwrap();
    assert!(msg(&pair.internal).contains("change descriptor"));
    let good = c.descriptor_with_checksum().unwrap();
    let damaged = format!("{}#qqqqqqqq", good.split('#').next().unwrap());
    assert!(msg(&damaged).contains("checksum doesn't match"));
    assert_eq!(msg("tr(xpub,sortedmulti_a(2,a,b))"), TAPROOT);
    // Testnet file on mainnet, and the reverse, say which way round.
    let t = two_of_three(Network::Testnet);
    for format in ExportFormat::ALL {
        let file = t.export(format, Network::Testnet).unwrap();
        let e = MultisigConfig::import(&file.content, n).unwrap_err();
        assert!(matches!(e, Error::Network(_)), "{format:?}: {e}");
        assert!(e.to_string().contains("test network"), "{e}");
    }
    let e = MultisigConfig::import(&good, Network::Testnet).unwrap_err();
    assert!(
        matches!(e, Error::Network(_)) && e.to_string().contains("mainnet"),
        "{e}"
    );
}

// ---------------------------------------------------------------------------
// One cosigner from a device export.

/// BlueWallet fixture: an older Coldcard multisig export (`p2wsh_p2sh`).
const COLDCARD_MULTISIG_EXPORT: &str = r#"{"p2sh_deriv":"m/45'","p2sh":"xpub6847W6cYUqq4ixcmFb83iqPtJZfnMPTkpYiCsuUybzFppJp2qzh3KCVHsLGQy4WhaxGqkK9aDDZnSfhB92PkHDKihbH6WLztzmN7WW9GYpR","p2wsh_p2sh_deriv":"m/48'/0'/0'/1'","p2wsh_p2sh":"Ypub6kvtvTZpqGuWtQfg9bL5xe4vDWtwsirR8LzDvsY3vgXvyncW1NGXCUJ9Ps7CiizSSLV6NnnXSYyVDnxCu26QChWzWLg5YCAHam6cYjGtzRz","p2wsh_deriv":"m/48'/0'/0'/2'","p2wsh":"Zpub75mAE8EjyxSzoyPmGnd5E6MyD7ALGNndruWv52xpzimZQKukwvEfXTHqmH8nbbc6ccP5t2aM3mws3pKYSnKpKMMytdbNEZFUxKzztYFM8Pn","xfp":"168DD603"}"#;

/// BlueWallet fixture: current Coldcard / Unchained multisig export (`h`
/// paths, `p2sh_p2wsh`).
const UNCHAINED_EXPORT: &str = r#"{
    "xfp": "B68AF6E4",
    "account": 0,
    "p2wsh_deriv": "m/48h/0h/0h/2h",
    "p2wsh": "Zpub74w9dfoeurKrKXE3SPRpFquLPTkiCuSwGuhDzBgbE42w5ShB2FxMjmJyjZpSJ6WhLt8y1PeFHQELGgq2GmktviFDH8yFWYRWg4xQiw3v335",
    "p2sh_deriv": "m/45h",
    "p2sh": "xpub69EKPNo9Jkd6v2h7xNKw5RdbFBoaHEcstXcRNfcQ2jg71iFpobCwcxfJjaV2ycGy218f2jM1znqs1SDkqMiR7fbyBVJwzacg2QarGt1gtJg",
    "p2sh_p2wsh_deriv": "m/48h/0h/0h/1h",
    "p2sh_p2wsh": "Ypub6k6tL18jmAnNRGZpk4u3WPGDmWMkdZNmx3MySYdQywCwMMHqNoKHeqLAgU6pFokHKQFdi88vAW4g3TEsCAymoq5LnFXd54RkQ8m3AD9f81J"
}"#;

/// BlueWallet fixture: Coldcard Q generic export (trimmed `desc` fields).
/// Each section's `xfp` is that node's own fingerprint, not the master's.
const COLDCARD_Q_GENERIC: &str = r#"{
  "chain": "BTC",
  "xfp": "B68AF6E4",
  "account": 0,
  "xpub": "xpub661MyMwAqRbcGbfPtiuiikr1CCP6c1FktqmZDttdUdAgYRAoioWKarWcCnZP5cwi5WGx3iyQdWv7Zs4UQ413GuGr3KyLcsvVsWBCqbxN5az",
  "bip44": { "name": "p2pkh", "xfp": "9671C307", "deriv": "m/44h/0h/0h", "xpub": "xpub6CMpeBXdEKC3kxA96LeyvJgxQfPbxXuJu91wVcLjYKMcSf1dMr9HEkNUydPTxM1SkmutnYjFikr27JUtUSqP9gq1299SP7eKrKFr9YjsrPE", "first": "1DFavpj8xohigXrXwk1PpdF3rwTeHk494L" },
  "bip84": { "name": "p2wpkh", "xfp": "A208A33F", "deriv": "m/84h/0h/0h", "xpub": "xpub6DMiK1d7crjcan79RD6ZSTGyNCKP8RS95eioBCKf7ReyZFpoxiW2qMvLCZdNC9WiMgwyMMcLwFTxir5C4bK5aRnXgh9pAM1fjjxC1HpE6Hf", "_pub": "zpub6s2EvLxwvDpaHNVP5vfordTyi8cH1fR8usmEjz7RsSQjfTTGU2qA5VEcEyYYBxpZAyBarJoTraB4VRJKVz97Au9jRNYfLAeeHC5UnRZbz8Y", "first": "bc1q8tart7tv6da6ylqauf2fde23vwgpsakxznkf7c" },
  "bip48_1": { "name": "p2sh-p2wsh", "xfp": "20F3FB3F", "deriv": "m/48h/0h/0h/1h", "xpub": "xpub6ENXu6jP3XgX9QDKy3eSUDpuskB3UahgifC5jtTzE9zeg4uhMjmSwep6rZBkhTsTgJ4r64YEpdKd311YBFQos7H55Sio5k9GEgRXBhYNZvm", "_pub": "Ypub6k6tL18jmAnNRGZpk4u3WPGDmWMkdZNmx3MySYdQywCwMMHqNoKHeqLAgU6pFokHKQFdi88vAW4g3TEsCAymoq5LnFXd54RkQ8m3AD9f81J" },
  "bip48_2": { "name": "p2wsh", "xfp": "95397F29", "deriv": "m/48h/0h/0h/2h", "xpub": "xpub6ENXu6jP3XgXCMgRq1Pb1bNXKjRZ7JnM8R17W8dH6GSmM4VokYExQX8mtSwnjqyxJ8qNdrT1Us8jNwz8Y9mvBkmLhzTzwKKYEtZFMszFScb", "_pub": "Zpub74w9dfoeurKrKXE3SPRpFquLPTkiCuSwGuhDzBgbE42w5ShB2FxMjmJyjZpSJ6WhLt8y1PeFHQELGgq2GmktviFDH8yFWYRWg4xQiw3v335" },
  "bip45": { "name": "p2sh", "xfp": "B4282109", "deriv": "m/45h", "xpub": "xpub69EKPNo9Jkd6v2h7xNKw5RdbFBoaHEcstXcRNfcQ2jg71iFpobCwcxfJjaV2ycGy218f2jM1znqs1SDkqMiR7fbyBVJwzacg2QarGt1gtJg" }
}"#;

const B68_P2WSH_XPUB: &str = "xpub6ENXu6jP3XgXCMgRq1Pb1bNXKjRZ7JnM8R17W8dH6GSmM4VokYExQX8mtSwnjqyxJ8qNdrT1Us8jNwz8Y9mvBkmLhzTzwKKYEtZFMszFScb";

#[test]
fn cosigner_from_device_exports() {
    let n = Network::Bitcoin;
    let abandon = key(ABANDON, n, "m/48'/0'/0'/2'");
    // SeedSigner / Specter-DIY / Sparrow text: `h` or `'`, xpub or Zpub.
    for text in [
        "[73c5da0a/48h/0h/0h/2h]Zpub74Jru6aftwwHxCUCWEvP6DgrfFsdA4U6ZRtQ5i8qJpMcC39yZGv3egBhQfV3MS9pZtH5z8iV5qWkJsK6ESs6mSzt4qvGhzJxPeeVS2e1zUG".to_string(),
        "[73c5da0a/48h/0h/0h/2h]xpub6DkFAXWQ2dHxq2vatrt9qyA3bXYU4ToWQwCHbf5XB2mSTexcHZCeKS1VZYcPoBd5X8yVcbXFHJR9R8UCVpt82VX1VhR28mCyxUFL4r6KFrf".to_string(),
        format!("  [73C5DA0A/48'/0'/0'/2']{}/0/*\n", abandon.xpub),
        // Coldcard's `p2wsh_desc` template.
        format!("wsh(sortedmulti(M,[73c5da0a/48h/0h/0h/2h]{}/0/*,...))", abandon.xpub),
    ] {
        assert_eq!(cosigner_from_export(&text, n).unwrap(), abandon, "{text}");
    }
    assert_eq!(abandon.xpub, "xpub6DkFAXWQ2dHxq2vatrt9qyA3bXYU4ToWQwCHbf5XB2mSTexcHZCeKS1VZYcPoBd5X8yVcbXFHJR9R8UCVpt82VX1VhR28mCyxUFL4r6KFrf");

    // Coldcard / Passport / Keystone JSON prefers the P2WSH key.
    let k = cosigner_from_export(COLDCARD_MULTISIG_EXPORT, n).unwrap();
    assert_eq!(k.fingerprint, "168dd603");
    assert_eq!(k.path, "m/48'/0'/0'/2'");
    let zpub2 = "Zpub75mAE8EjyxSzoyPmGnd5E6MyD7ALGNndruWv52xpzimZQKukwvEfXTHqmH8nbbc6ccP5t2aM3mws3pKYSnKpKMMytdbNEZFUxKzztYFM8Pn";
    assert_eq!(k.xpub, descriptors::normalize_slip132(zpub2).unwrap());

    let k = cosigner_from_export(UNCHAINED_EXPORT, n).unwrap();
    assert_eq!(
        (k.fingerprint.as_str(), k.path.as_str()),
        ("b68af6e4", "m/48'/0'/0'/2'")
    );
    assert_eq!(k.xpub, B68_P2WSH_XPUB);

    // Generic export: master fingerprint from the top level.
    let k = cosigner_from_export(COLDCARD_Q_GENERIC, n).unwrap();
    assert_eq!(k.fingerprint, "b68af6e4");
    assert_eq!(k.xpub, B68_P2WSH_XPUB);

    // Cobo / Keystone `{xfp, xpub, path}`, escaped slashes and all.
    let cobo = r#"{"xfp":"D37EAD88","xpub":"Zpub74ijpfhERJNjhCKXRspTdLJV5eoEmSRZdHqDvp9kVtdVEyiXk7pXxRbfZzQvsDFpfDHEHVtVpx4Dz9DGUWGn2Xk5zG5u45QTMsYS2vjohNQ","path":"m\/48'\/0'\/0'\/2'"}"#;
    let k = cosigner_from_export(cobo, n).unwrap();
    assert_eq!(
        (k.fingerprint.as_str(), k.path.as_str()),
        ("d37ead88", "m/48'/0'/0'/2'")
    );

    // Only a nested key on offer: take it.
    let nested_only = r#"{"xfp":"B68AF6E4","p2sh_p2wsh_deriv":"m/48h/0h/0h/1h","p2sh_p2wsh":"Ypub6k6tL18jmAnNRGZpk4u3WPGDmWMkdZNmx3MySYdQywCwMMHqNoKHeqLAgU6pFokHKQFdi88vAW4g3TEsCAymoq5LnFXd54RkQ8m3AD9f81J"}"#;
    assert_eq!(
        cosigner_from_export(nested_only, n).unwrap().path,
        "m/48'/0'/0'/1'"
    );

    // Testnet keys (tpub / Vpub) on testnet.
    let t = key(ABANDON, Network::Testnet, "m/48'/1'/0'/2'");
    assert_eq!(
        cosigner_from_export(&t.key_expression(), Network::Testnet).unwrap(),
        t
    );
}

#[test]
fn cosigner_export_errors() {
    let n = Network::Bitcoin;
    let msg = |text: &str| cosigner_from_export(text, n).unwrap_err().to_string();
    // Single-sig exports.
    let mut single: Value = serde_json::from_str(COLDCARD_Q_GENERIC).unwrap();
    for s in ["bip48_1", "bip48_2", "bip45"] {
        single.as_object_mut().unwrap().remove(s);
    }
    assert_eq!(msg(&single.to_string()), SINGLE_SIG_EXPORT);
    let cobo_single = r#"{"ExtPubKey":"zpub6rcabYFcdr41zyUNRWRyHYs2Sm86E5XV8RjjRzTFYsiCngteeZnkwaF2xuhjmM6kpHjuNpFW42BMhzPmFwXt48e1FhddMB7xidZzN4SF24K","MasterFingerprint":"5271c071","AccountKeyPath":"84'\/0'\/0'","CoboVaultFirmwareVersion":"1.2.4(BTC-Only)"}"#;
    assert_eq!(msg(cobo_single), SINGLE_SIG_EXPORT);
    assert_eq!(
        msg("[8cce63f8/84h/0h/0h]zpub6s2RJ9qAEBW8Abhojs6LyDzF7gttcDr6EsR3Umu2aptZBb45e734rGtt4KqsCMmNyR1EEzUU2ugdVYez2VywQvAbBjUSKn8ho4Zk2c5otkk"),
        SINGLE_SIG_EXPORT
    );
    // A whole wallet.
    let wallet = two_of_three(n).descriptor_with_checksum().unwrap();
    assert_eq!(msg(&wallet), WHOLE_WALLET);
    assert_eq!(msg(SPARROW_COLDCARD), WHOLE_WALLET);
    // No origin, nothing at all, garbage.
    assert_eq!(
        msg("Zpub74ijpfhERJNjhCKXRspTdLJV5eoEmSRZdHqDvp9kVtdVEyiXk7pXxRbfZzQvsDFpfDHEHVtVpx4Dz9DGUWGn2Xk5zG5u45QTMsYS2vjohNQ"),
        MISSING_ORIGIN
    );
    assert_eq!(msg("asdfasdgsqwrgqwegq"), NO_KEY);
    assert!(msg("").contains("Nothing to import"));
    // An Electrum keystore without a fingerprint.
    let no_fp = r#"{"keystore":{"xpub":"Zpub74ijpfhERJNjhCKXRspTdLJV5eoEmSRZdHqDvp9kVtdVEyiXk7pXxRbfZzQvsDFpfDHEHVtVpx4Dz9DGUWGn2Xk5zG5u45QTMsYS2vjohNQ","derivation":"m/48'/0'/0'/2'"}}"#;
    assert_eq!(msg(no_fp), MISSING_ORIGIN);
    // Keys from two devices in one paste.
    let two = format!(
        "{}\n{}",
        key(ABANDON, n, "m/48'/0'/0'/2'").key_expression(),
        key(ZOO, n, "m/48'/0'/0'/2'").key_expression()
    );
    assert_eq!(msg(&two), WHOLE_WALLET);
    // Wrong network.
    let t = key(ABANDON, Network::Testnet, "m/48'/1'/0'/2'");
    assert!(matches!(
        cosigner_from_export(&t.key_expression(), n),
        Err(Error::Network(_))
    ));
    // Path shallower than the key.
    assert!(msg("[73c5da0a/48h/0h/2h]xpub6DkFAXWQ2dHxq2vatrt9qyA3bXYU4ToWQwCHbf5XB2mSTexcHZCeKS1VZYcPoBd5X8yVcbXFHJR9R8UCVpt82VX1VhR28mCyxUFL4r6KFrf").contains("doesn't match its xpub"));
}

#[test]
fn first_address_and_checksum() {
    let n = Network::Bitcoin;
    let c = two_of_three(n);
    let d = c.descriptor_with_checksum().unwrap();
    let parsed = Descriptor::<DescriptorPublicKey>::from_str(&d).unwrap();
    assert!(parsed.is_multipath());
    let receive = parsed.into_single_descriptors().unwrap().remove(0);
    let first = receive
        .at_derivation_index(0)
        .unwrap()
        .address(n.to_bitcoin())
        .unwrap()
        .to_string();
    assert_eq!(c.first_address(n).unwrap(), first);
    assert!(first.starts_with("bc1q"));
    let t = two_of_three(Network::Testnet);
    assert!(t
        .first_address(Network::Testnet)
        .unwrap()
        .starts_with("tb1q"));
    assert!(matches!(t.first_address(n), Err(Error::Network(_))));
}

#[test]
fn export_format_serde_is_camel_case() {
    assert_eq!(
        serde_json::to_string(&ExportFormat::Bsms).unwrap(),
        "\"bsms\""
    );
    assert_eq!(
        serde_json::from_str::<ExportFormat>("\"coldcard\"").unwrap(),
        ExportFormat::Coldcard
    );
    let f = ExportFile {
        filename: "a.txt".into(),
        mime: "text/plain".into(),
        content: "x".into(),
    };
    assert_eq!(
        serde_json::to_value(&f).unwrap(),
        json!({"filename": "a.txt", "mime": "text/plain", "content": "x"})
    );
}
