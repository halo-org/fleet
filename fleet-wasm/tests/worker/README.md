# Worker test component

This fixture exports an ordinary prompt provider and a private worker from the
same component. Fresh prompt calls exchange bytes with a persistent echo worker.
It also exercises startup failure and shutdown of a CPU-bound worker.

Run the host tests from the repository root:

```sh
cargo test -p fleet-wasm wasm_workers
```

The tests build this fixture offline in `target/worker-fixture` and encode it as a
component. Install the `wasm32-unknown-unknown` Rust target and ensure its linker
is available. Fetch workspace dependencies before running offline.
