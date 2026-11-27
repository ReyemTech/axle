# Contributing to Axle

Axle is source-available under the [Elastic License 2.0](LICENSE).

## Contributor license agreement

Every contributor signs the [CLA](CLA.md) once. When You open a pull request, a bot comments
with instructions; reply with:

> I have read the CLA Document and I hereby sign the CLA

Signatures are stored on the `cla-signatures` branch. The CLA lets ReyemTech relicense
contributions, which is required because ReyemTech alone offers Axle as a hosted service.

## How to contribute

1. Open an issue first for anything bigger than a small fix.
2. Fork, branch from `main`, and keep each pull request focused.
3. Use conventional commit messages (`feat:`, `fix:`, `docs:`, `ci:`).
4. Run the checks below, then open the pull request.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# Postgres-backed tests: needs a reachable Postgres
AXL_TEST_PG_URL=postgres://postgres:x@localhost:5432/postgres cargo test --features pg-tests
```
