# Project Guidelines

## Commits

This project enforces [Conventional Commits](https://www.conventionalcommits.org/) via commitlint + Husky.

All commit messages must follow the format:

```
type(scope): description
```

Where `type` is one of: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.

Scope is optional. Description must be lowercase and imperative (e.g., "add storage trait", not "Added storage trait").

Do not add `Co-Authored-By` lines to commit messages.

## Code Quality

Pre-commit hooks run `cargo fmt --check` and `cargo clippy -- -D warnings`. All code must pass both before committing.
