Program binaries dumped from mainnet with `solana program dump` (program-data account, header stripped), then cut at the end of the ELF: the bytes after it are zero padding.

| file | program | dumped | last deployed slot |
|---|---|---|---|
| programs/reflect_main.so | RFLCTEKcjxbMx2s9XJVTX4nWnGiCw3kT6VewvJTzKam | 2026-11-14 | 457,319,440 |
| programs/kamino_lend.so | KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD | 2026-11-14 | 461,744,249 |
| programs/kamino_farms.so | FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr | 2026-11-14 | 444,035,168 |
| programs/jupiter_lend.so | jup3YeL8QhtSx1e253b2FDvsMNC87fDrgQZivbrndc9 | 2026-11-14 | 453,954,884 |
| programs/jupiter_liquidity.so | jupeiUmn818Jg1ekPURTpr4mFo29p46vygyykFJ3wZC | 2026-11-14 | 453,953,579 |

`reflect_main.so` also serves the bonds suites.

`snapshots/lending.json` holds every account that a quote, a mint and a redeem of lending strategy `7hHkpUXHXudwSE7HKctjntJLmJM82sZ3wqzWvJevTJch` read, all at slot 469,381,220.
It carries the slot and the block time it was read at.

Rewrite it from mainnet:

```bash
RPC_URL=<url> cargo test -p reflect-titan-lending --test parity capture_snapshot -- --ignored
```

A venue refresh fails on a stale oracle, so the tests run at the snapshot's own clock.
A program dump that is newer than the snapshot can refuse the old state: dump the programs again when you rewrite the snapshot.
