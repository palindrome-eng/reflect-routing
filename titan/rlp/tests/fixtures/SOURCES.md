Program binaries: one dumped from mainnet (program-data account, header stripped), one built locally.

| file | program | source |
|---|---|---|
| programs/rlp_prod.so | JrXLmS6aYJNJDVxdAfjNJE5wikT8ubf3TA9iL2JA9Av | mainnet dump 2026-09-29 after the upgrade at slot 451,679,568 (authority `28sR8pmJ…`): `feat/oracle-stacking` at or after 8910b83 (carries `migrate_asset`, `post_oracle_price`, `reclaim_cooldown`, no `update_protected_vault`), macOS-built; exact commit not yet matched by instruction multiset |
| programs/rlp_stacking.so | built locally 2026-09-29 | reflect-program-library `feat/oracle-stacking` @ 9b63c07 (the branch tip), platform-tools v1.52 |

`snapshots/rlp_prod.json` holds the mainnet accounts of pool 4 (settings, pool, LP mint, the migrated 266 B asset, mint, oracle, vault) at slot 451,683,280.

The branch does not carry 85a6eaf (`initialize_lp` takes `Option<Pubkey>` for `protected_vault`); the SDK builder sends a bare pubkey, which the program reads as the `None` tag when its first byte is zero. The bootstrap here relies on that.

`drift.rs` probes the `PoolWipedOut` deposit gate the stacking build added; its test is ignored until the SDK models it.
