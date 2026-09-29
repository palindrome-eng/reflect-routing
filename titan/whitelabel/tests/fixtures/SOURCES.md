Program binaries dumped from mainnet (program-data account, header stripped).

| file | program | dumped | source |
|---|---|---|---|
| programs/proxy_prod.so | pRoxYU64BSjv8HbhENna8a7LVCrkzzNrnvbYuTwas8C | 2026-09-28 | reflect-proxy-program `main` @ b2a3562, `--features prod` |
| programs/proxy_staging.so | HvX27r2MoKn6oakkmcAgMo5ZFhheGKQ5Ht2KL3sk5ANM | 2026-09-29 | reflect-proxy-program `feat/oracle-stacking` @ 3350057, `--features staging` (upgrade slot 451,622,568; identified by instruction multiset against a local build) |

`snapshots/proxy_prod.json` holds one prod market over a 98 B Pyth asset (slot 451,286,499); `snapshots/proxy_staging.json` one staging market over a 259 B two-leg Pyth asset (slot 451,662,341). Each carries the slot and timestamp it was read at.

`drift.rs` probes the gates the audit fixes added to the staging build; its tests are ignored until the SDK models them.
