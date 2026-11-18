# entitle — repo guide

Self-hosted RevenueCat alternative: Axum + PostgreSQL (sqlx) + tokio. One binary + Postgres, no other infra.

## Commands

`just serve` (entitle + dashboard on :8080 against Postgres on localhost:5432),
`just check`. Underneath:

```sh
docker compose up -d postgres                          # dev/test DB on 127.0.0.1:55433 (compose project `entitle`)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                 # #[sqlx::test] creates a DB per test
docker build -t entitle .
cargo test -p entitle-server --lib export_bindings    # regenerate sdk/js/src/generated (ts-rs)
```

`DATABASE_URL` for tests comes from `.cargo/config.toml` (overridable by env).

## Layout

- `crates/entitle-core` — pure domain: types, `compute_entitlements`, login/merge + ownership rules
  (`identity`), event derivation (`events::diff_events`), the adapter traits (`adapter`),
  revenue normalization (`billing`: periods + monthly amounts, `fx`: ECB rates + conversion),
  and revenue metrics from purchase history (`metrics`).
  No HTTP, no DB. `testing` feature = snapshot builders.
- `crates/entitle-{creem,apple,google}` — store adapters. Implement `StoreAdapter`
  (+ `CheckoutAdapter` for Creem). No DB access. Contract: `docs/adapters.md`.
- `crates/entitle-server` — binary `entitle`: config, auth, routes (`routes/{admin,sdk,webhooks}.rs`),
  persistence (`purchases.rs`, `customers.rs`), outbox worker (`outbox.rs`), ECB rate worker (`fx.rs`), crypto, adapter wiring
  (`adapters.rs`). Migrations in `crates/entitle-server/migrations`, embedded and run on startup.
  Dashboard (`dashboard/`): Topcoat pages at `/dashboard`, signed in with the admin token via a
  signed HttpOnly cookie. Reads use the admin routes' functions, setup writes call the admin API
  in-process (`dashboard/api.rs`); charts are uPlot (`assets/dashboard/chart.js`). After a build,
  `topcoat asset bundle --package entitle-server` puts the assets next to the binary.
- `docs/api.md` — HTTP contract for the npm SDK. Update it with every API change.
- `sdk/js` — `@oxidt/entitle`, the TypeScript client SDK (Bun). Wire types in `src/generated` come
  from the Rust types via ts-rs (`ts` feature of entitle-core, `cfg_attr(test)` in the server).

## Conventions

- sqlx without compile-time macros: `query_as::<_, T>` + `FromRow`. Dynamic SQL only via
  `sqlx::AssertSqlSafe` with constants interpolated, never user input.
- Enums are stored as text using their serde names (`db::enum_str` / `db::parse_enum`).
- Adapters return state (`PurchaseSnapshot`), never events. The server derives events.
- Ordering: a snapshot with older `as_of` never overwrites newer state. Inbound events are
  deduplicated by `(project, store, event_id)` in the same transaction as the state change.
- Outgoing webhooks are written to `outbox` in the same transaction; the worker claims rows with
  `FOR UPDATE SKIP LOCKED` + a `locked_until` lease in a short transaction, sends without holding
  locks, and retries with exponential backoff.
- Secrets at rest (store credentials, webhook secrets): XChaCha20-Poly1305 with row-bound AAD.
  API keys: SHA-256 hash only. Never log secrets (credential types have redacting `Debug`).
- Data minimisation: store app user ids and store ids only; no emails or names.
- Errors: `ApiError` → `{"error": {"code", "message"}}`.
- Server integration tests use the fake adapter in `crates/entitle-server/tests/common`.
