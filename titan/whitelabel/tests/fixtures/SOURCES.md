Program binaries dumped from mainnet (program-data account, header stripped).

| file | program | dumped | upgrade slot |
|---|---|---|---|
| programs/proxy_prod.so | pRoxYU64BSjv8HbhENna8a7LVCrkzzNrnvbYuTwas8C | 2026-09-28 | |
| programs/proxy_staging.so | HvX27r2MoKn6oakkmcAgMo5ZFhheGKQ5Ht2KL3sk5ANM | 2026-09-29 | 451,622,568 |

`snapshots/proxy_prod.json` holds one prod market (slot 451,286,499); `snapshots/proxy_staging.json` one staging market (slot 451,662,341). Each carries the slot and timestamp it was read at.

`drift.rs` proves the staging build's gates (staleness in seconds, undercollateralised wrap, commission-adjusted pricing) from both sides.
