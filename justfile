# Local development against a Postgres on localhost:5432 with role/database `entitle`. Dev-only values, as in
# docker-compose.yml; the environment (or a .env you load yourself) overrides them.

export DATABASE_URL := env("DATABASE_URL", "postgres://entitle:entitle@localhost:5432/entitle")
export ENTITLE_ADMIN_TOKEN := env("ENTITLE_ADMIN_TOKEN", "dev-admin-token-change-me-0123456789abcdef")
export ENTITLE_ENCRYPTION_KEY := env("ENTITLE_ENCRYPTION_KEY", "ZGV2LW9ubHktZW5jcnlwdGlvbi1rZXktMzJieXRlcyE=")
export ENTITLE_BIND := env("ENTITLE_BIND", "127.0.0.1:8080")
export ENTITLE_PUBLIC_URL := env("ENTITLE_PUBLIC_URL", "http://localhost:8080")

default:
    @just --list

# entitle with the dashboard at http://localhost:8080/dashboard/ (rebuilds and reloads on change).
serve:
    topcoat dev --package entitle-server --bin entitle

fmt:
    cargo fmt --all
    topcoat fmt --rustfmt crates/entitle-server/src/dashboard

# What CI checks (tests create a database per test on the same server).
check:
    cargo fmt --all --check
    topcoat fmt --check --rustfmt crates/entitle-server/src/dashboard
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
