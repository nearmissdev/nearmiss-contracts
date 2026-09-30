# NEARMISS contracts

Smart contracts behind [nearmiss.fun](https://nearmiss.fun): 3,333 incident files from the Near-Miss Investigation Board, on NEAR.

| Contract | Account | What it does |
|---|---|---|
| `nearmiss/` | `nearmissboard.near` | The collection. Every file sits in one pool; `nft_mint_random` draws from the files still in the archive using the receipt's random seed and removes the pick, so each file is issued once. 0.12 NEAR per file, up to 5 per call, partial fills refunded. NEP-171, 177, 178, 181, 199, 297. |
| `market/` | `market.nearmissboard.near` | Fixed-price listings (through `nft_approve`) and escrowed offers for approved collections. Every sale settles through `nft_transfer_payout`; the buyer is refunded in full if the transfer fails. |
| `factory/` | `launchpad.nearmissboard.near` | The launchpad. A creator reserves `<slug>.launchpad.nearmissboard.near` with a deposit; `deploy` then creates the account, deploys the collection code and initialises it in one batch, refunding the deposit if any step fails. The launchpad stores only the SHA-256 of the collection code, so `deploy` must pass the exact bytes. Until then the creator can cancel for a full refund. |
| `collection/` | one per launchpad collection | A creator's collection: random or in-order mints, optional start time, royalty up to 7.5% to the creator, 2.5% platform fee on each mint. |

## Build

Every crate builds reproducibly with [cargo-near](https://github.com/near/cargo-near):

```
cd nearmiss && cargo near build reproducible-wasm
cd market   && cargo near build reproducible-wasm
cd collection && cargo near build reproducible-wasm
cd factory  && cargo near build reproducible-wasm
```

The Docker image and digest are pinned in each `Cargo.toml`, so anyone can rebuild the exact bytes that are deployed and compare them with the code hash on chain.

## Test

```
cd nearmiss && cargo test                  # unit tests, including drawing all 3,333 files
cd nearmiss && cargo test --test sandbox   # compiled wasm in a local NEAR sandbox
cd market   && cargo test --test sandbox   # list, buy, offers, refunds, with the collection contract
cd collection && cargo test                # unit tests
cd factory  && cargo test --test sandbox   # reserve, deploy, mint, fee split, refunds, cancel
```

## Key rules

- The draw cannot be steered: the file is chosen inside the minting receipt from `env::random_seed()`, and NEAR receipts are final, so a caller cannot mint and roll back a result it does not like.
- Mint proceeds, minus the storage each token uses, go to the treasury account. Royalty on secondary sales is 5% (NEP-199).
- `freeze_media` makes the image and metadata locations permanent. It cannot be undone.

## License

MIT
