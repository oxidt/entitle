# entitle

Self-hostable, EU-friendly alternative to RevenueCat. One Rust binary plus
PostgreSQL unifies a customer's entitlements across web (Creem as Merchant of
Record), the App Store (StoreKit 2) and Google Play.

- Multi-project: one instance serves several apps, each with its own keys and store credentials.
- HTTP API. Contract: [`docs/api.md`](docs/api.md).
- Dashboard at `/dashboard/` (sign in with the admin token), built into the binary: setup
  (projects, API keys, products, entitlements, store credentials, webhooks), revenue
  metrics and purchase activity.
- Store adapters: [`docs/adapters.md`](docs/adapters.md).
- Store setup to a first sandbox purchase: [`docs/store-setup.md`](docs/store-setup.md); example app: [`examples/capacitor`](examples/capacitor).
- Status: foundation and the Creem, Apple and Google adapters are implemented.

## Should you use it?

**Can you use it?** Absolutely. It's open source, runs as one binary next to Postgres, and
handles App Store, Google Play and web purchases.

**Should you use it?** Probably not, at least not yet. I built entitle for my own apps, so it
follows my requirements: web payments go through Creem, and other providers like Stripe or
Paddle aren't supported. If your setup looks like mine, go for it.

**Will I host it for you?** Probably not. Run it yourself, it's built for that.

**Will I accept pull requests?** Yes, see [CONTRIBUTING.md](CONTRIBUTING.md). For bigger
changes, open an issue first so we can talk about the approach.

## Run

```sh
docker compose up -d postgres            # Postgres on 127.0.0.1:55433

export DATABASE_URL=postgres://entitle:entitle@127.0.0.1:55433/entitle
export ENTITLE_ADMIN_TOKEN=$(openssl rand -hex 32)
export ENTITLE_ENCRYPTION_KEY=$(openssl rand -base64 32)
cargo run --bin entitle                  # listens on ENTITLE_BIND (default 0.0.0.0:8080)
```

Or everything in Docker (the same two secrets are required; keep them, the key decrypts stored credentials):

```sh
export ENTITLE_ADMIN_TOKEN=$(openssl rand -hex 32)
export ENTITLE_ENCRYPTION_KEY=$(openssl rand -base64 32)
docker compose --profile app up --build
```

The compose Postgres password is a dev default; change it before exposing the database.
Prebuilt images: `ghcr.io/oxidt/entitle:latest` (every commit on `main` is also tagged with its sha).

| Variable | Required | Meaning |
|---|---|---|
| `DATABASE_URL` | yes | Postgres URL; migrations run on startup |
| `ENTITLE_ADMIN_TOKEN` | yes | bearer token for `/admin/v1`, min. 32 chars |
| `ENTITLE_ENCRYPTION_KEY` | yes | base64 of 32 bytes; encrypts store credentials and webhook secrets. Losing it makes them unreadable. |
| `ENTITLE_BIND` | no | listen address, default `0.0.0.0:8080` |
| `ENTITLE_WEBHOOK_ALLOW_PRIVATE` | no | `true` lets outgoing webhooks target any http(s) host, incl. the private network (`http://backend:8080`). Default: public https hosts only. |
| `ENTITLE_PUBLIC_URL` | no | public origin, e.g. `https://entitle.example.com`; enables the MCP endpoint `/mcp` and its OAuth server (see `docs/api.md`) |
| `ENTITLE_TRUST_PROXY` | no | `true` behind a reverse proxy that sets `X-Forwarded-For`: rate limits then key on its last entry instead of the TCP peer |
| `RUST_LOG` | no | log filter, default `info` |
| `ENTITLE_LOG_FORMAT` | no | `json` for JSON logs |

## First project

`entitle admin` is a client for the admin API of a running server (`--url` or
`ENTITLE_URL`, default `http://localhost:8080`; token from `ENTITLE_ADMIN_TOKEN`).
Projects are addressed by id or name.

```sh
entitle admin projects create "My App" --currency EUR
entitle admin keys create "My App"                    # pk_… for the app, printed once
entitle admin entitlements create "My App" pro
entitle admin products create "My App" com.example.pro.monthly --store app_store --billing-period P1M
entitle admin entitlements attach "My App" pro com.example.pro.monthly
entitle admin credentials set-apple "My App" --issuer-id … --key-id ABC123 \
  --key-file SubscriptionKey_ABC123.p8 --bundle-id com.example.app
entitle admin credentials set "My App" play_store play.json   # body as in docs/api.md
entitle admin customers grant "My App" user_42 pro --days 30  # support: also get, revoke
```

With Docker: `docker compose exec entitle entitle admin …` (the container already
has the token). Webhook endpoints are set with the HTTP API (see `docs/api.md`).
Point the stores at `/webhooks/{creem|apple|google}/{project_id}`.

## Development

`just serve` runs entitle with the dashboard at http://localhost:8080/dashboard/ against a
Postgres with an `entitle` role and database on localhost:5432 (admin token
`dev-admin-token-change-me-0123456789abcdef`), rebuilding on change. `just check` runs what CI runs.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                   # needs the compose Postgres running
```

The dashboard is server-rendered with [Topcoat](https://github.com/tokio-rs/topcoat)
(`crates/entitle-server/src/dashboard/`; charts use uPlot from
`crates/entitle-server/assets/dashboard/`). Its hashed assets are bundled into `assets/` next
to the binary; without them `/dashboard/` shows a build hint.

```sh
cargo install topcoat-cli --version 0.12.1 --locked
cargo build --bin entitle && topcoat asset bundle --package entitle-server
topcoat fmt --rustfmt crates/entitle-server/src/dashboard   # formats view! markup too
```

## License

The server and its crates are licensed under [AGPL-3.0-only](LICENSE). The client SDKs in
[`sdk/js`](sdk/js) and [`sdk/capacitor`](sdk/capacitor) are MIT. See
[CONTRIBUTING.md](CONTRIBUTING.md) to contribute and [SECURITY.md](SECURITY.md) to report a
vulnerability.
