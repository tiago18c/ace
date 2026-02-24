> [!WARNING]
> **This is a toy/research project. It is unaudited, incomplete, and not suitable for production use. Do not deploy with real funds.**

# ace program template

This repository has a template for asynchronous solana programs capable of supporting full application controlled execution (ACE) on-chain

The `core` crate has the traits, and `counter` has an example implementor where decrements are prioritized before increments.

Run `cargo run --example counter` from the `counter` directory after building the program with `cargo-build-sbf` to see it in action.

The `perps` crate is a more complete example — see [perps/README.md](perps/README.md) for details, examples, and analysis.

# Disclaimer

All of this code is unaudited and partially vibe coded so use at your own risk
