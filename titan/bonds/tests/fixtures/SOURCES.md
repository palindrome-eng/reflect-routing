The program binary is `../../../lending/tests/fixtures/programs/reflect_main.so`, the mainnet dump that the lending suite documents.

`snapshots/bond_basket.json` holds every account that a quote and each instruction of index strategy `9PugLepDrG7H5rThZM5bZPJGbRg3Pf6WmDk6vU2yb4dk` read, all at slot 469,381,982.
It carries the slot and the block time it was read at.

Rewrite it from mainnet:

```bash
RPC_URL=<url> cargo test -p reflect-titan-bonds --test parity capture_snapshot -- --ignored
```

On mainnet the strategy holds under one dollar, all of it in one pool.
`parity_mainnet_state_swept` runs on that state as it is.
The other execution tests first mint 100,000 tokens of each asset through the program, so that every direction has depth.
