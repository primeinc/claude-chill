# Contributing

Contributions welcome! Feel free to open issues or pull requests.

## Bug Reports

When filing a bug report, please include:

1. The output of `claude-chill --version`:
   ```
   $ claude-chill --version
   claude-chill 0.1.5 (b73342e)
   ```
2. Debug logs from `claude-chill -v <your command>` (logs go to stderr)

## Branch Naming

- `feature/description`
- `fix/description`

## CI Checks

CI runs the following on pull requests:

- `cargo fmt --check` — formatting
- `cargo clippy -- -D warnings` — linting (Ubuntu + Windows)
- `cargo test --all-targets` — tests (Ubuntu, macOS, Windows)
- `cargo audit` — dependency vulnerability scanning
- `cargo-deny` — license compliance and supply chain verification
- Version badge check — README badge must match Cargo.toml version
- CHANGELOG check — CHANGELOG.md must have an entry for the current version

## Before Submitting

1. Run `cargo fmt --all`
2. Run `cargo clippy --all-targets --all-features -- -D warnings`
3. Run `cargo test --all-targets`
4. Update `CHANGELOG.md` if your change is user-facing
5. Update version in both `Cargo.toml` AND `README.md` if bumping

## Security

For security-sensitive changes, review the [THREAT_MODEL.md](THREAT_MODEL.md)
and ensure new code paths are covered by the STRIDE analysis. All `unsafe`
blocks must have `// SAFETY:` comments documenting their invariants.
