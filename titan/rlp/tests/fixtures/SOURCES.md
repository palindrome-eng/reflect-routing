Program binaries: one dumped from mainnet (program-data account, header stripped), one built locally.

| file | program | source |
|---|---|---|
| programs/rlp_prod.so | JrXLmS6aYJNJDVxdAfjNJE5wikT8ubf3TA9iL2JA9Av | reflect-program-library `feat/protected-vault-required` @ 85a6eaf, dumped 2026-09-28 |
| programs/rlp_stacking.so | built locally 2026-09-29 | reflect-program-library `feat/oracle-stacking` @ 9b63c07 merged with 85a6eaf (the deployed protected-vault commit the branch lacks), platform-tools v1.52 |

`snapshots/rlp_prod.json` holds the mainnet accounts of pool 4 (settings, pool, LP mint, asset, mint, oracle, vault) at slot 451,286,496.

`drift.rs` probes the `PoolWipedOut` deposit gate the stacking build added; its test is ignored until the SDK models it.
