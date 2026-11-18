# syntax=docker/dockerfile:1
# Build: docker build -t entitle .
FROM rust:1.99-trixie AS build
# Bundles the dashboard's assets (crates/entitle-server/assets) next to the binary.
RUN cargo install topcoat-cli --version 0.11.0 --locked
WORKDIR /src
COPY . .
# Served at /version so a deploy can be checked against a commit.
ARG GIT_SHA
ENV ENTITLE_GIT_SHA=$GIT_SHA
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin entitle \
    && topcoat asset bundle --release --package entitle-server \
    && cp target/release/entitle /entitle \
    && cp -r target/release/assets /assets

# glibc + libgcc only, runs as uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /entitle /usr/local/bin/entitle
# The dashboard loads its asset bundle from ./assets next to the binary.
COPY --from=build /assets /usr/local/bin/assets
ENV ENTITLE_BIND=0.0.0.0:8080
EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/entitle"]
