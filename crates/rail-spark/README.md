# rail-spark

The Spark / Lightning spending account for Sessions Wallet. It implements `sessions_core::Rail` on top of the [Breez SDK – Spark](https://github.com/breez/spark-sdk), pinned to release tag **0.26.1**.

| | Methods |
|---|---|
| Receive | Spark address (or a Spark invoice when an amount is given), BOLT11 invoice, Bitcoin address (static deposit address) |
| Pay | Spark address and Spark invoice, BOLT11, Lightning address, LNURL-pay, Bitcoin address (cooperative exit) |

**Not supported:**
- BOLT12. SDK 0.26.1 can parse offers but can't pay them.
- Token payments and conversions.
- Custom sat/vB on withdrawals. Fast, normal and slow map to the SDK's fast, medium and slow quotes.
- External PSBT signing.

## API key

On mainnet you need a Breez API key. It's free: request one at <https://breez.technology/request-api-key/>. Pass it as `SparkConfig::api_key`. Regtest works without one.

## Keys

`SparkConfig::mnemonic` takes the BIP85 child phrase (`keystore::child::SPARK`), never the master phrase. The SDK derives keys from it at `m/8797555'/{1 on mainnet, 0 on regtest}'`.

## Storage

Pass `storage_dir` as a directory that holds only this account. The SDK's built-in SQLite storage writes to:

```
<storage_dir>/<network>/<first 8 hex chars of sha256(identity pubkey)>/storage.sql   (+ -wal, -shm)
```

That one file holds payments, deposits, cached state and the Spark leaf tree, including the data needed for unilateral exit. Funds are always recoverable from the phrase. Deleting the file only loses local metadata, such as LNURL descriptions, and forces a full resync.

## Chain backend

`SparkConfig::chain` picks where the SDK reads the chain (deposit detection, claims, refunds, claim-fee rates). `SparkChain::Electrum` serves it from the app's shared Electrum connection (`chain::ElectrumChainService`), `SparkChain::Esplora` from your own Esplora or mempool server, and `SparkChain::Default` keeps the SDK's built-in mempool.space. Test networks always use the SDK's default.

## Notes

- Send quotes are kept in memory, so a `SendQuote` can only be used in this process and doesn't survive a restart. Its id doubles as the SDK idempotency key, so retrying a failed `send` can't pay twice.
- A Lightning send waits up to 20 s for settlement and returns `Pending` if it hasn't settled by then.
- On-chain deposits are claimed automatically once they have 3 confirmations, if the claim fee is at or below the network's next-block rate plus 1 sat/vB. Until then they show as `pending_in`. `SparkAccount::deposits()` lists them with their status; `claim_deposit` claims one with a fee the user approved (or early, for the provider's spread), and `refund_deposit` sends one back on-chain.
- Call `SparkAccount::disconnect()` before you drop an account.
