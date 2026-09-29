Program binaries: one dumped from mainnet (program-data account, header stripped), one built locally.

| file | program | source |
|---|---|---|
| programs/rlp_prod.so | JrXLmS6aYJNJDVxdAfjNJE5wikT8ubf3TA9iL2JA9Av | mainnet dump 2026-09-29 after the upgrade at slot 451,679,568 (authority `28sR8pmJ…`): `feat/oracle-stacking` at or after 8910b83 (carries `migrate_asset`, `post_oracle_price`, `reclaim_cooldown`, no `update_protected_vault`), macOS-built; exact commit not yet matched by instruction multiset |
| programs/rlp_stacking.so | built locally 2026-09-29 | reflect-program-library `feat/oracle-stacking` @ 9b63c07 (the branch tip), platform-tools v1.52 |

`snapshots/rlp_prod.json` holds the mainnet accounts of pool 4 (settings, pool, LP mint, the migrated 266 B asset, mint, oracle, vault) at slot 451,683,280.

`drift.rs` proves the `PoolWipedOut` deposit gate the stacking build added, from both sides.
