# Contributing to ishtaria-server

Thank you. ishtaria-server is the authoritative world server (Rust, axum, SQLx, PostgreSQL): accounts, the world, every gameplay rule, federation.

1. Read the [developer guide](https://github.com/VitexSoftware/ishtaria-docs/tree/main/source/development): `invariants` (short, binding), `local-setup`, `cookbook`.
2. Open an issue for anything larger than a fix; architectural changes are written as a decision record first.
3. Make a small change with a test that fails without it (also for the refusal paths) and run the checks:

   ```sh
   cargo fmt --check
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked
   # with PostgreSQL (SQLx makes a throw-away database per test; never use a database you care about)
   DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql' \
   ISHTARIA_TEST_HEIGHTMAP="$PWD/../ishtaria-worldgen/examples/planet-seed42-256.pgm" \
     cargo test --locked -- --include-ignored
   ```

4. Use Conventional Commits (`feat(ishtaria-server): ...`), branch from `main`, and fill in the pull request template:
   what you tested, what you could not test, what is still planned.

Working with Claude Code is welcome and expected: `CLAUDE.md` in this repository is read automatically. You are
responsible for the code you submit; read the diff, especially SQL, locking, validation and every place a client
value reaches a server.

By contributing you agree to license your work under AGPL-3.0-only (see `LICENSE`).
