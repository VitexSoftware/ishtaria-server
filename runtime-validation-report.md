# Runtime Validation

Date: 2026-10-04

## Environment

- PostgreSQL 17.11 on the local Unix socket; real database, no mocks.
- Rust 1.95.0. Dependencies resolved for the declared Rust 1.80 minimum;
  compilation with Rust 1.80 itself was not verified.
- API-only server; browser rendering tests are not applicable.
- Integration tests use isolated databases managed by SQLx.

## Results

| Check | Result | Evidence |
| --- | --- | --- |
| Locked build | PASS | `cargo build --locked` exited 0 |
| Lint | PASS | `cargo clippy --locked --all-targets -- -D warnings` exited 0 |
| Unit and database tests | PASS | `cargo test --locked -- --include-ignored`: 5 passed, 0 failed, 0 skipped |
| Startup and migrations | PASS | HTTP listener on port 7400; migration 1 recorded as successful |
| Import | PASS | `examples/planet-seed42-256.pgm`, seed 42, face size 256 |
| HTTP roundtrip | PASS | `curl --fail .../world/heightmap` piped into `cmp` matched the source |
| Real process restart | PASS | Restart without `--import`; `/health` and byte-exact download passed |

PostgreSQL stores one world and one heightmap: 393232 PGM bytes and 393216
decoded pixel bytes. Source SHA-256:
`cd2d55014eaa82c797dba524c5605f45bb08cbe16a87bf020d5fb968d6636709`.

The tests also verify idempotent import, conflicting seed/map/ruleset rejection,
rollback of invalid imports, all six face indices, coordinate boundaries,
read-only HTTP access and a 503 health response when the pool is unavailable.

The workspace task `Ishtaria: verify PostgreSQL server` repeats the live HTTP
roundtrip and database inspection without an interactive pager.

## Limits

Gameplay simulation, authentication, federation, accounts and cell deltas are
not implemented. The persisted PGM is a finite terrain preview. This validation
does not certify production load, remote database TLS or Debian package builds.
