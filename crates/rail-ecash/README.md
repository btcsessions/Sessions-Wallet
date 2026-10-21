# rail-ecash

A Cashu ecash account for Sessions Wallet. Each account is tied to one mint and holds sats. It is built on CDK `cdk 0.18.2` (`wallet` feature) and stores its data in `cdk-redb 0.18.2`.

## Trust model

Ecash is custodial. The mint holds the real bitcoin. This account only holds bearer tokens (blind-signed proofs) that the mint has promised to redeem. A mint can refuse to redeem, go offline, get hacked or simply disappear, and those funds are gone. Blind signatures mean the mint can't link a deposit to a later spend. Even so, the mint sees every Lightning invoice it pays or receives, and it can see your IP unless you use Tor (not enabled here). The local database holds the proofs themselves, so anyone who copies the file can spend them. The proofs come from the wallet's BIP85 child seed (`keystore::child::ECASH`). When the account's database is new (a new device, or a restore from the recovery phrase), the first `sync` runs a NUT-09/13 restore scan against the mint, so the phrase alone recovers any unspent ecash, as long as the mint is still online and supports restore. Tokens you created but nobody has claimed yet are recovered too. Treat this account as a spending wallet for small amounts.

## What it does

| `Rail` method | Implementation |
|---|---|
| `open` | BIP39 seed of the supplied child phrase becomes the CDK wallet seed. Unit is sat. Opens or creates the redb file. No network access. |
| `info` | `confirmed` is the unspent proofs. `pending_in` is quotes paid but not yet minted. `pending_out` is proofs that are pending, reserved, or sent but not yet claimed. Custody is `Mint { url }`. Receives and pays BOLT11. |
| `receive(Bolt11)` | NUT-04 mint quote. Returns the invoice, `watch_id` = quote id, and `expires_at`. An amount is required. |
| `sync` | `recover_incomplete_sagas` (crash recovery), then `mint_unissued_quotes`, then `finalize_pending_melts`, then checks whether handed-out tokens have been claimed. |
| `prepare_send(Bolt11)` | NUT-05 melt quote. `fee_sat` = fee reserve, `handle` = quote id. Checks the balance first. Any other destination returns `Unsupported`. |
| `send` | `prepare_melt` + `confirm`. Unused fee reserve comes back as NUT-08 change. `fee_sat` is the fee actually paid. |
| `redeem(CashuToken)` | Rejects a token from a different mint *before touching it*: `Unsupported("token is from another mint: <url>")`. Otherwise swaps it into fresh proofs. |
| `create_token` | `prepare_send` (`include_fee`, so the recipient nets the full amount) + `confirm`. Returns `Pending` until claimed. |
| `payments` | CDK's own transaction log in the same redb file. There is no separate log. |

## Not done

- Reclaiming an unclaimed token (`revoke_send`) is not wired up.
- BOLT12, P2PK-locked tokens, multi-mint tokens, and Tor are not supported.
- `payments()` returns no encoded token for past sends, because CDK doesn't keep it once the send completes.

## Notes

- cdk-redb 0.18.2 never creates its `wallet_sagas` table on a fresh file, so `sync` fails until the first saga is written. `ensure_redb_tables` creates the table before opening.
- cdk-sqlite is avoided: its rusqlite clashes with the Spark SDK's `libsqlite3-sys`. For the same reason the real CDK `Mint` can't be used in tests: its only DB backends are sqlite and postgres. The tests use `src/fake_mint.rs` instead, an in-process `MintConnector` with real BDHKE signatures, double-spend tracking and a hand-driven fake Lightning side.
