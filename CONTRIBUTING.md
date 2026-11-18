# Contributing

Open an issue before you start a large change. [DESIGN.md](DESIGN.md) says what Nisaba should become and in what order.

## Tools

- Rust: `rust-toolchain.toml` picks the version and adds the `wasm32-unknown-unknown` target.
- `wasm-bindgen-cli` at the version of `wasm-bindgen` in `Cargo.lock`.
- [Bun](https://bun.sh) 1.3 and Node 22 or newer.

## Checks

CI runs these. Run them before you open a pull request:

```sh
node scripts/build-wasm.mjs
cd web
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun run test
bun run build
cd ..
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The server embeds `web/dist-server`, so build the web client before you test the server.

The measurements in the pull request text come from a test that is off by default:

```sh
cargo test --release -p nisaba-server --test measure -- --ignored --nocapture
```

On Linux, `cargo test` includes `crates/nisaba-server/tests/sandbox.rs`, which fails on a kernel that cannot sandbox exports: one older than 6.2, or without Landlock or seccomp. It does not skip.

## Rules

- `crates/nisaba-server/src/api/` describes the HTTP API: `types.rs` holds the JSON bodies and `routes.rs` every route. When you change either, or a close code or limit, run `NISABA_WRITE_API=1 cargo test -p nisaba-server --lib api` and commit `api/openapi.json` and `web/src/app/api.gen.ts`. CI fails when they are stale.
- A new route needs an entry in `routes.rs` and a handler in `crates/nisaba-server/src/http/mod.rs`; the conformance test `every_route_answers_as_described` then calls it.
- Put a new fixed limit in `crates/nisaba-server/src/limits.rs`, and test it at its edge.
- Add a test for each change in behaviour.
- Commit no build output, `.env` files or secrets.

## Licence

Nisaba is under the AGPL-3.0-only licence ([LICENSE](LICENSE)). You license your contribution under the same terms.
