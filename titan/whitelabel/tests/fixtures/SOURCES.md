Program binaries dumped from mainnet on 2026-09-28 (program-data account, header stripped), copied from reflect-core-sdk `tests/fixtures` at v0.11.0.

| file | program | source |
|---|---|---|
| programs/proxy_prod.so | pRoxYU64BSjv8HbhENna8a7LVCrkzzNrnvbYuTwas8C | reflect-proxy-program `main` @ b2a3562, `--features prod` |
| programs/proxy_staging.so | HvX27r2MoKn6oakkmcAgMo5ZFhheGKQ5Ht2KL3sk5ANM | reflect-proxy-program `feat/oracle-stacking` @ 8b71e4e, `--features staging` |

`snapshots/proxy_prod.json` holds one prod market over a 98 B Pyth asset; `snapshots/proxy_staging.json` one staging market over a 259 B two-leg asset. Each carries the slot and timestamp it was read at.
