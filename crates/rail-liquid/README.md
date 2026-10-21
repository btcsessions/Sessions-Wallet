# rail-liquid: Boltz swaps

`BoltzSwaps` implements `sessions_core::SwapProvider`. The user never holds
L-BTC: Liquid is only ever the far side of a swap.

| Bridge | Swap | What we do |
|---|---|---|
| pay `LiquidAddress` with `Bolt11` | reverse (LN → L-BTC) | Our Lightning account pays Boltz's hold invoice; we claim L-BTC straight to the target address. |
| pay `LiquidAddress` with `BitcoinAddress` | chain (BTC → L-BTC) | Our on-chain account locks BTC; we claim L-BTC to the target. If it fails, `tick` refunds the BTC to the `refund_address`. |
| receive `LiquidAddress` into `Bolt11` | submarine (L-BTC → LN) | The outside sender pays a Boltz lockup address (`liquidnetwork:` BIP21 with amount); Boltz pays our invoice. |

There's no L-BTC → BTC chain-swap receive. Claiming it is fine, but refunding a failed one would need a Liquid wallet, and we don't have one.

Amounts: the payer is charged so the destination gets **exactly** the
requested amount, after Boltz's percentage, Boltz's lockup fee and our claim fee.
The claim fee is reserved for a script-path claim at 0.1 sat/vB. Boltz's min and max limits are enforced, and its quote is
checked against the advertised fees before anything payable is returned.

Library: `boltz-client` pinned to SatoshiPortal/boltz-rust trunk `7542d2e`
(Sep 2026). It's newer than 0.4.1 and adds timelock, lockup-txid, address-network and
dust validation. Every release of that crate depends on the **yanked**
`secp256k1 0.32.0-beta.2` (MuSig2). Cargo only selects it through the entry in the
workspace `Cargo.lock`, so use `cargo update -p <crate>` and never a bare
`cargo update`.

## Trust assumptions

- **Boltz is non-custodial but must be online and honest about liveness.** Every
  lockup is a Taproot HTLC that we validate against our own keys and preimage
  hash before journaling the swap. Boltz can delay a swap, but it can't take funds.
- **Preimage safety.** The preimage is only revealed (cooperative claim or
  script-path claim) after *our* Esplora shows Boltz's lockup confirmed, so a
  zero-conf double-spend can't trick us. Once revealed, a chain swap is always
  claimed, never refunded.
- **Submarine receive:** we co-sign Boltz's sweep only after verifying its
  preimage pays our invoice (`submarine_cooperative_claim`).
- **Chain data** comes from Esplora (default blockstream.info, configurable).
  Broadcasts fall back to Boltz's broadcast endpoint if Esplora is down.
- **Privacy:** Boltz sees invoices, amounts and addresses for each swap, and
  Esplora sees the addresses we query.

## Recovery

- Keys: the swap mnemonic (BIP85 child `SWAPS` of the wallet seed) is used as a
  Boltz **rescue key**. Keys are `m/44/0/0/0/{i}`, preimage = `sha256(key i)`. The
  same phrase works on <https://boltz.exchange/rescue/external>, and Boltz's
  `/swap/restore` API can list every swap it made for it.
- Before the first swap in each process, `next_index` is synced with Boltz's
  restore index, so a restored wallet never reuses a key or preimage.
- Journal: `data_dir/boltz-swaps.json` (mode 0600, atomic temp + fsync +
  rename). Each swap is written before its invoice or address is returned. Every
  signed claim or refund is written before broadcast, so `tick` is idempotent and
  resumes after a crash: it rebroadcasts, re-signs if rejected, and bumps stuck BTC refunds after 6 h.
- **Failed incoming (submarine) swaps** hold someone else's L-BTC. They go to
  state `refund_required`. Call `BoltzSwaps::refund_to_liquid(id, address)` with the
  sender's Liquid address, or use the Boltz rescue page with the swap phrase.
  `tick` marks the swap refunded or complete once the lockup is emptied.
