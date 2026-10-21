# rail-liquid: Boltz swaps

`BoltzSwaps` implements `sessions_core::SwapProvider`. The user never holds
L-BTC: Liquid is only ever the far side of a swap. The same provider also lets
an on-chain account (savings, hardware, multisig, watch-only) receive from
Lightning or Liquid: the BTC is claimed to one of that account's own addresses,
so no key of the account is ever needed.

| Bridge | Swap | What we do |
|---|---|---|
| pay `LiquidAddress` with `Bolt11` | reverse (LN → L-BTC) | Our Lightning account pays Boltz's hold invoice; we claim L-BTC straight to the target address. |
| pay `LiquidAddress` with `BitcoinAddress` | chain (BTC → L-BTC) | Our on-chain account locks BTC; we claim L-BTC to the target. If it fails, `tick` refunds the BTC to the `refund_address`. |
| receive `LiquidAddress` into `Bolt11` | submarine (L-BTC → LN) | The outside sender pays a Boltz lockup address (`liquidnetwork:` BIP21 with amount); Boltz pays our invoice. |
| receive `Bolt11` into `BitcoinAddress` | reverse (LN → BTC) | The outside payer pays Boltz's hold invoice; Boltz locks BTC; we claim it to the account's address. |
| receive `LiquidAddress` into `BitcoinAddress` | chain (L-BTC → BTC) | The outside sender locks L-BTC at Boltz's lockup (`liquidnetwork:` BIP21); Boltz locks BTC; we claim it to the account's address. |

`Wallet::receive` prefers Liquid → Lightning for accounts that take invoices
(Spark) and only uses the on-chain bridges for accounts that don't.

Amounts, pay bridges: the payer is charged so the destination gets **exactly** the
requested amount, after Boltz's percentage, Boltz's lockup fee and our claim fee.
The Liquid claim fee is reserved for a script-path claim at 0.1 sat/vB.

Amounts, BTC receive bridges: the user enters what the **payer pays** (invoice
amount / L-BTC sent). Boltz's percentage, Boltz's miner fee and our BTC claim
fee come out of it. The claim fee in the quote is an estimate (Esplora 3-block
rate, ~125 vB); the real claim is signed at the fee rate when it happens, so the
amount that lands can differ slightly from the quote. `receive_quote` reports
live limits and fees before anything is created.

Boltz's min and max limits (live pair data, never hardcoded: BTC reverse swaps
start around 25k sats) are enforced, and its quote is checked against the
advertised fees before anything payable is returned.

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
  claimed, never refunded. For BTC receives that means the payer's Lightning
  payment stays pending until Boltz's BTC lockup has one confirmation
  (typically 10–60 minutes).
- **Chain swap co-signing.** A cooperative L-BTC → BTC claim partial-signs
  Boltz's sweep of the sender's L-BTC (with our refund key) in exchange for
  Boltz's signature on our BTC claim. The script-path fallback needs nothing
  from Boltz.
- **Wrong L-BTC amount.** If the sender locks a different amount, Boltz offers
  a new BTC amount. We unblind the sender's lockup ourselves and accept only a
  quote that matches Boltz's advertised fees for what was actually locked;
  otherwise the swap waits until it expires and then needs a rescue (below).
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
  resumes after a crash: it rebroadcasts, re-signs if rejected, bumps stuck BTC
  refunds after 6 h and stuck BTC claims after 1 h (claims are RBF-enabled, +1
  sat/vB or the current 2-block estimate, capped at 500 sat/vB).
- **Failed incoming swaps from Liquid** (submarine, and L-BTC → BTC chain
  swaps whose sender locked L-BTC but Boltz never locked BTC, or the amount was
  wrong and no fair quote came) hold someone else's L-BTC behind *our* refund
  key. They go to state `refund_required`. Call
  `BoltzSwaps::refund_to_liquid(id, address)` with the sender's Liquid address,
  or use the Boltz rescue page with the swap phrase. `tick` marks the swap
  refunded or complete once the lockup is emptied.

## The app closed mid-swap

Swaps only move while the app is unlocked (`tick` every ~10 s).

- **LN → BTC (reverse).** Before we claim, nothing is at risk: if we're away
  until Boltz's swap expires, Boltz cancels the hold invoice and the payer's
  Lightning payment fails back to them. Once our claim is signed the preimage is
  out (Boltz settles the invoice), so the claim must confirm before Boltz's BTC
  timeout (about a day). `tick` rebroadcasts and bumps it; if the app stays closed
  past the timeout with the claim unconfirmed, Boltz could refund its lockup and
  the payment would be lost. Keep the app open until the swap shows `complete`.
- **L-BTC → BTC (chain).** Same claim-side rule. If we're away so long that
  Boltz refunds its BTC lockup before we claimed, the sender's L-BTC is still in
  its lockup and the swap goes to `refund_required` for a refund to the sender.
- The Liquid lockup address for an L-BTC → BTC receive is shown for at most half
  of the sender's refund window (capped at 24 h). Payments after that may fail and
  need the rescue path.
