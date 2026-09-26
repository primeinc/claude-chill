# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `-v`/`--verbose` flag for release-mode debug logging to stderr
- `verbose` config option for persistent logging without CLI flag
- `RUST_LOG` environment variable support for granular log control
- `#![deny(unsafe_op_in_unsafe_fn)]` lint for stricter unsafe hygiene
- `#![warn(clippy::undocumented_unsafe_blocks)]` lint to enforce SAFETY comments
- `// SAFETY:` comments on all unsafe blocks across the codebase
- `THREAT_MODEL.md` — formal STRIDE threat analysis per Microsoft SDL
- `CHANGELOG.md` — structured release history
- `deny.toml` — cargo-deny configuration for license and supply chain checks
- `cargo-deny` CI job for automated license compliance
- CHANGELOG version check in CI (must match Cargo.toml)
- 5 new e2e tests: verbose flag, special characters, nonzero exit codes, help, version
- 2 new config tests: verbose default and TOML parsing
- Windows config path documented in README
- Troubleshooting section in README
- Windows-specific attack surface section in THREAT_MODEL.md
- Cargo.toml metadata: keywords, categories, rust-version
- Platform-specific config path assertions in tests (Windows/macOS/Linux)
- Info-level startup logging with config summary

### Fixed
- Sync buffer overflow: hard cap at 1 MiB prevents unbounded memory growth from
  malicious child processes that never send `SYNC_END`
- Pre-existing benchmark compilation error in `render.rs` (return of borrowed reference)
- Pre-existing clippy warning in `render.rs` (useless `format!`)

### Changed
- Logging refactored: stderr logging available in release builds; file-write remains debug-only
- CONTRIBUTING.md updated with CI checks, debug logging instructions, and security guidance
- SECURITY.md cross-references THREAT_MODEL.md
- README documents verbose flag, Windows config path, and troubleshooting

## [0.1.5] - 2025-06-01

### Fixed
- Security hardening: whitelist-based escape sequence filter with explicit
  classification of every `termwiz::Action` variant (no catch-all fallbacks)
- Deduplication of lookback sequence handling between legacy and Kitty protocol

### Changed
- Improved input validation: lookback sequences validated at spawn time with
  16-byte length cap

## [0.1.4] - 2025-05-01

### Added
- Windows support via ConPTY pseudo-console
- Cross-platform E2E tests proving Windows proxy works
- Threaded pipe reader for Windows event loop

### Changed
- CI matrix expanded to include Windows for check, clippy, and test jobs

## [0.1.3] - 2025-04-01

### Changed
- Extracted `VtRenderer` and `HistoryManager` from monolithic `Proxy`
- Extracted `SequenceMatcher`, `AltScreenTracker`, and platform-independent
  modules from `proxy.rs`
- Extracted terminal utilities into dedicated `terminal.rs`

### Added
- `#[must_use]` annotations on pure public functions
- Debug assertions and input validation
- Comprehensive integration tests for `VtRenderer` and `HistoryManager`
- Stress and edge-case tests for `HistoryManager`, `SyncBlockParser`
- Fuzz targets for `sync_block`, `history_filter`, `key_parser`
- Module documentation on all source files
- Doc comments on all public items

### Fixed
- Sync block handling before alt screen enter
- Backward compatibility for removed `refresh_rate` config field
- Panic in `sequence_match::check` with empty sequence
- Ghostty terminal support

### Performance
- Fast-path `HistoryFilter` returning `Cow::Borrowed` when all actions are safe
- Pre-allocated segment Vec in `process_output` hot path
- Removed unnecessary 1 MiB re-allocation after sync block flush
- Pre-allocated `history_filter` output buffer with input capacity

## [0.1.2] - 2025-03-01

### Fixed
- Filtered Kitty keyboard and other terminal queries from history
- Removed terminal queries from history to prevent terminal responses on stdin
- Fixed Kitty keyboard protocol detection and tracking
- Fixed auto-lookback trigger timing

### Added
- Auto-lookback mode with configurable idle timeout (`-a` flag)
- Version tracking in CLI output via build script

## [0.1.1] - 2025-02-01

### Added
- VT100 emulator-based differential rendering
- Nix package support (`flake.nix`)

### Fixed
- Exit display cleanup on process termination
- macOS compatibility (Ctrl+Shift+6 for lookback key)

## [0.1.0] - 2025-01-01

### Added
- Initial release
- PTY proxy with synchronized output block interception (DEC mode 2026)
- Scrollback history buffer with configurable max lines
- Configurable lookback key via CLI (`-k`) and config file
- TOML configuration file support (`~/.config/claude-chill.toml`)
- CI workflow with format, lint, and test checks
