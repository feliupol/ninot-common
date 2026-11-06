# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow the calendar: `<year>.<ISO week>.<n>`.

## [2026.45.0] - 2026-11-05

### Added

- Shared building blocks for in-memory fakes of external services, each behind a cargo feature (`server`, `tls`, `git`, `build`, `profile`, `state`); the default build enables none.
- `server`: bind a port and serve an axum router on a background runtime until dropped.
- `tls`: serve HTTPS with a throwaway certificate authority, so clients can be pointed at a trusted local CA.
- `git`: bare repositories served over git's smart HTTP protocol.
- `faults`: a rule engine for deliberate misbehaviour (error statuses, latency, dropped or truncated responses), generic over each fake's own target and fault types, with per-rule probability, occurrence counts and a seed for reproducible runs.
- `router` and `operation`: match request paths against an operation table, with route templates that mix parameters and literals within a segment.
- `spec` (feature `build`): generate the operation table from a pinned API description in a build script.
- `profile`: record the requests a client makes and report the ones it repeats.
- `state`: a shared state handle with versioned snapshots that can be taken from one server and served by another.
- `token`: unique, randomly generated tokens.
- Requires Rust 1.88 or newer (edition 2024).
