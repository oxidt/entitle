# Contributing

Issues and pull requests are welcome. For larger changes, open an issue first so we can agree
on the approach.

## Setup

See [Development](README.md#development). Before you open a pull request:

```sh
just check   # fmt, clippy -D warnings, tests (needs Postgres, see README)
```

- Update [`docs/api.md`](docs/api.md) with every API change.
- Regenerate the SDK wire types after changing them:
  `cargo test -p entitle-server --lib export_bindings`.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/).

## License

By contributing you agree that your contribution is licensed under the license of the part of
the repository it touches: AGPL-3.0-only for the server and its crates, MIT for `sdk/`.
