## What and why

<!-- What does this change, and what problem does it solve? Link the issue: Fixes #123 -->

## How it was tested

<!-- Commands you ran, new tests, manual checks in the UI or a harness. -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Store changes are covered on both SQLite and Postgres (`cargo test --features pg-tests`)
- [ ] Docs and `CHANGELOG.md` (`[Unreleased]`) are updated for user-visible changes
- [ ] No tokens, secrets or personal data in code, tests, screenshots or logs
- [ ] I have signed the [CLA](../CLA.md)
