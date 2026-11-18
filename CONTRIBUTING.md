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

## Rules

- `crates/nisaba-server/src/api.rs` defines the HTTP API types. When you change them, run `NISABA_WRITE_API=1 cargo test -p nisaba-server api` and commit `web/src/app/api.gen.ts`.
- Add a test for each change in behaviour.
- Commit no build output, `.env` files or secrets.

## Licence

Nisaba is under the AGPL-3.0-only licence ([LICENSE](LICENSE)). You license your contribution under the same terms.
