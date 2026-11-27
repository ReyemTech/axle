# Contributing to Axle

Axle is source-available under the [Elastic License 2.0](LICENSE). Taking part means
following the [code of conduct](CODE_OF_CONDUCT.md). Report security problems privately as
described in [SECURITY.md](SECURITY.md), not in an issue.

## Contributor license agreement

Every contributor signs the [CLA](CLA.md) once. When You open a pull request, a bot comments
with instructions; reply with:

> I have read the CLA Document and I hereby sign the CLA

Signatures are stored on the `cla-signatures` branch. The CLA lets ReyemTech relicense
contributions, which is required because ReyemTech alone offers Axle as a hosted service.

## How to contribute

1. Open an issue first for anything bigger than a small fix.
2. Fork, branch from `main`, and keep each pull request focused.
3. Use conventional commit messages (`feat:`, `fix:`, `docs:`, `ci:`; `feat!:` for breaking changes). release-plz builds the version bump and changelog from them.
4. Update the docs under [`docs/`](docs/README.md) and the `[Unreleased]` section of
   [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.
5. Run the checks below, then open the pull request.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# Postgres-backed tests: needs a reachable Postgres
AXL_TEST_PG_URL=postgres://postgres:x@localhost:5432/postgres cargo test --features pg-tests
bash tests/helm.sh     # Helm chart: lint, render and assert every mode (needs helm)
bash tests/smoke.sh    # Docker image end to end (needs docker)
```

## Where things are

[docs/architecture.md](docs/architecture.md) maps the source tree. Design records for larger
changes live in [docs/superpowers/](docs/README.md#design-records); new phases get a design
doc there before they are built (see [ROADMAP.md](ROADMAP.md)).
