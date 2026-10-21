# Sessions Wallet

A self-custodial bitcoin wallet you build yourself. It starts as a dead-simple spend-and-save wallet; add modules whenever you want more:

| Module | What it gives you | Built on |
|---|---|---|
| Spending | Instant Lightning and Spark payments | Breez SDK – Spark |
| Savings | Plain on-chain bitcoin (BIP84) | BDK |
| Vault | Multisig (`wsh(sortedmulti)`, BIP48 keys) | BDK |
| Hardware wallet | Coldcard, Jade, BitBox02, Trezor, Passport, Keystone… via PSBT | BDK |
| Liquid | Pay or get paid by a Liquid address, swapped automatically | Boltz |
| Ecash | Cashu tokens from a mint you pick, send sats as a link | CDK |
| Move | Shift sats between any two accounts in one step | core planner |
| Watch-only, fees, price | Readouts | |

Runs on macOS, Windows, Linux, iOS, Android and GrapheneOS (no Google services needed) from one codebase.

> **Status: early alpha on mainnet.** The code is unaudited. Use small amounts you can afford to lose until it has been reviewed.

## How it fits together

```
apps/wallet/            Tauri 2 app: React UI (src/) + native shell (src-tauri/)
crates/sessions-core    Types, keystore, request parser, move planner, Wallet
crates/rail-onchain     On-chain accounts (BDK): hot, multisig, hardware, watch-only
crates/rail-spark       Spending account (Breez SDK – Spark)
crates/rail-liquid      Liquid swaps (Boltz): no L-BTC is ever held
crates/rail-ecash       Cashu ecash accounts (CDK)
```

**Accounts, methods and moves.** Every account says which payment methods it can receive and pay (bitcoin address, BOLT11, Spark address, Liquid address…). To pay something or move between accounts, the planner matches the payer's methods with the receiver's, routing through a swap provider or a hub account (usually Spending) when there's no direct path. The UI shows the route and the exact fee before you confirm.

**One recovery phrase.** On-chain accounts use the master phrase directly, so it also restores in Sparrow, Nunchuk and others. Spark, ecash and swap keys each come from their own BIP85 child phrase, so a bug in one SDK can't expose on-chain keys, while the one master phrase still recovers everything.

**Keys at rest.** The phrase is encrypted with XChaCha20-Poly1305 under a key stretched from your passcode with Argon2id (64 MiB, 3 passes). Wallet databases store public descriptors only; signing happens in memory. Repeated wrong passcodes are rate-limited.

## Develop

Prerequisites: Rust 1.89+, Node 22+, `protoc` (Protocol Buffers compiler, used by the Spark SDK), and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS.

```sh
cd apps/wallet
npm install
npm run dev          # UI only, in the browser, with pretend sats
npm run tauri dev    # the real desktop app
```

Mobile:

```sh
npm run tauri android init && npm run tauri android dev
npm run tauri ios init && npm run tauri ios dev      # macOS + Xcode
```

The Spending account needs a free Breez API key ([request one](https://breez.technology/request-api-key/)). Build with `BREEZ_API_KEY=… npm run tauri build`, or paste it in Settings.

Tests:

```sh
cargo test --workspace
```

Dependency note: the Boltz client depends on a yanked `secp256k1` beta that is pinned in `Cargo.lock`. Update crates one at a time with `cargo update -p <crate>`; a bare `cargo update` will fail.

## Privacy

By default, on-chain data, fee estimates and prices come from mempool.space. Point the wallet at your own node (Umbrel, Start9, mempool) in Settings and your on-chain accounts talk only to it. Other modules necessarily talk to their own services: Spending to Spark operators and Breez, Liquid swaps to Boltz and a Liquid explorer, ecash to your chosen mint.

## License

MIT
