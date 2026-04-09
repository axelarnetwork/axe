# Project Guidelines

## Code Quality

Before committing any Rust code changes, always ensure:

1. **Formatting**: Run `cargo fmt` and fix any formatting issues.
2. **Linting**: Run `cargo clippy --workspace --all-targets -- -D warnings` and fix all warnings/errors.

All code must pass both checks with zero warnings before being committed.
