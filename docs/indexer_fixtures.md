# Event Indexer Compatibility Fixtures

This repository includes a dedicated fixture pack to ensure all smart contract events maintain stable, predictable payloads for downstream indexers and analytics pipelines.

## Running the Fixtures
Indexer maintainers can validate the current canonical event sequences by running the fixture test suite:

```bash
cargo test fixture_ -- --nocapture