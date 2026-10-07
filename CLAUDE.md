# ishtaria-server

The authoritative world server (Rust, axum, SQLx, PostgreSQL): accounts, the world, every gameplay rule, federation. Licence: AGPL-3.0-only. Part of the Ishtaria workspace of several repositories side by side
(`ishtaria-server`, `ishtaria-client`, `ishtaria-worldgen`, `ishtaria-core`, `ishtaria-content`, `ishtaria-protocol`,
`ishtaria-docs`): run Git, Cargo and `make` inside the repository you change. Work on `main`; do not commit,
push or deploy unless asked. Debian/Ubuntu x86-64 is the only supported platform.

Developer guide: https://github.com/VitexSoftware/ishtaria-docs/tree/main/source/development (`contributing`, `local-setup`, `invariants`, `cookbook`, the code tours).

## Checks before you say you are done

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# with PostgreSQL (SQLx makes a throw-away database per test; never use a database you care about)
DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql' \
ISHTARIA_TEST_HEIGHTMAP="$PWD/../ishtaria-worldgen/examples/planet-seed42-256.pgm" \
  cargo test --locked -- --include-ignored
```

## Where things are

* `src/main.rs` router and start-up; each gameplay area is a module with `routes()` (see `docs: server-tour`).
* `migrations/` append-only SQL; `etc/*.json|yaml` data the rules are made of; `src/tests/` one file per area.
* Adding things (items, recipes, spells, crops, shops, endpoints): `docs: cookbook`.

## Working notes

* Every action handler: token -> `players::player_id`, strict body struct, one transaction with
  `survival::lock_alive`, checks, commit, answer with `players::profile`.
* A new table needs constraints that hold the invariants, not only code. Never edit a released migration.

## Rules that never change (full text: docs `development/invariants`)

* The server decides; clients send intentions. Never trust a client value; validate every request and every answer.
* Related writes in one transaction; economy changes are atomic, bounded and repeat-safe.
* Migrations are append-only. Terrain is generated; only changes are stored. Never touch a live database in tests.
* A new character gets 100 gold exactly once. Death is permanent. Money buys space and appearance, not power.
* Language is a client preference: the server may receive it per request but never stores it.
* Content is data, not code. Federation is bilateral and signed.
* Never log or commit secrets. Argon2id passwords, hashed expiring tokens.
* Approved assets (Kenney, Quaternius, generated with the origin recorded); keep licence notices.
* Report honestly: what is implemented and tested, what is planned, what is blocked. Do not weaken a failing check.
