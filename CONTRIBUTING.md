# Contributing

git2 is an experimental learning project with a safety-sensitive goal. Contributions are welcome when they keep the supported scope explicit and add evidence rather than surface area.

## Before starting

Read:

- [README](README.md) (product scope and quickstart)
- [Architecture and invariants](docs/design.md)
- [Compatibility](docs/compatibility.md)
- [Roadmap](docs/roadmap.md)
- [Development guide](docs/development.md) (including test suites, testing rules, and CI workflow)

For on-disk changes, also read [Recovery format](docs/recovery-format.md) in full.

## Good contributions now

- tests for current compatibility claims;
- corruption and refusal coverage;
- non-UTF-8 path fixtures;
- documentation corrections;
- narrowly scoped work from the active `v0.2+` roadmap milestone;
- diagnostics that make a refusal safer or clearer;
- benchmarks that expose a concrete bottleneck without weakening correctness.

Large new commands, repository-feature expansion, and crate splits should begin with a design discussion tied to roadmap exit criteria.

## Pull request expectations

- Keep one coherent vertical slice per change.
- State supported behavior and explicit non-goals.
- Add differential tests when Git defines the behavior.
- Add failure-injection tests for mutations.
- Preserve byte-safe paths/content.
- Update all affected documentation and the changelog.
- Do not add ignored/placeholder tests or commands that panic.
- Do not create recovery refs.

Run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Safety bugs

Potential data-loss, path traversal, symlink traversal, corruption acceptance, or recovery-history publication issues should be treated as security-sensitive. Follow [SECURITY.md](SECURITY.md) rather than posting sensitive repository data publicly.

## License

By contributing, you agree that your contribution is licensed under the project’s MIT license.
