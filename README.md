# ninot-common

Common utilities for building Ninots: small, in-memory fake reproductions of
services, for use in tests. They provide the shared building blocks: the server
harness with optional TLS, the fault rule engine, bare git repositories over
smart HTTP, unique tokens, and the operation table generated from a pinned API
description.

Every part sits behind a cargo feature (`server`, `tls`, `git`, `build`); the
default build has none.

## Build and test

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## License

MIT
