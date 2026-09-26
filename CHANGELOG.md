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
- criterion benchmarks: `render` (VtRenderer full and diff) and `filter`
  (HistoryFilter, SyncBlockParser)
- cargo-fuzz targets: `fuzz_sync_block`, `fuzz_history_filter`, `fuzz_key_parser`
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

### Security
- anyhow 1.0.104 (RUSTSEC-2026-0190, unsound `Error::downcast_mut`)
- crossbeam-epoch 0.9.21 (RUSTSEC-2026-0204, via the criterion dev-dependency)

### Changed
- Logging refactored: stderr logging available in release builds; file-write remains debug-only
- CONTRIBUTING.md updated with CI checks, debug logging instructions, and security guidance
- SECURITY.md cross-references THREAT_MODEL.md
- README documents verbose flag, Windows config path, and troubleshooting

## [0.1.5] - 2026-03-26

### Added
- Windows support via ConPTY, with a threaded pipe reader for the event loop
- Windows in the CI check, clippy, and test matrix
- Cross-platform E2E tests that run the proxy on Windows
- `cargo-audit` CI job
- `SECURITY.md` with trust boundaries and disclosure process
- Nix package (#29)
- `cargo install --git` installation instructions (#31)
- Module docs on all source files and doc comments on all public items
- `#[must_use]` on pure public functions
- Tests for SyncBlockParser, KittyTracker, AltScreenTracker, sequence_match,
  config, key_parser, HistoryFilter, VtRenderer, HistoryManager, and proxy flow

### Changed
- `proxy.rs` split into platform-independent modules: SequenceMatcher,
  terminal utilities, `kitty_tracker.rs`, HistoryManager, VtRenderer, and
  `proxy_common.rs` (ProxyConfig, `should_auto_lookback`)
- Dead `escape_filter` removed
- Lookback sequences validated at spawn time, 16-byte cap
- `CLAUDE_CHILL_LOG_FILE` only in debug builds

### Removed
- `CLAUDE_CHILL_HISTORY_FILE`, which wrote terminal history to a caller-chosen path

### Fixed
- Windows command injection: arguments quoted with `CommandLineToArgvW`-compatible
  escaping (`quote_arg_windows`)
- Exit code truncation: `std::process::exit` instead of a `u8` cast
- Backward compatibility for the removed `refresh_rate` config field
- Sync block handling before alt screen enter
- Panic in `sequence_match::check` with an empty sequence
- `rerun-if-changed` paths in `build.rs` for the workspace layout
- `render_vt_screen` writes through `write_to_terminal`

### Performance
- No 1 MiB re-allocation after a sync block flush
- HistoryFilter returns `Cow::Borrowed` when every action is safe, and
  pre-allocates its output buffer
- Pre-allocated segment Vec in the `process_output` hot path

## [0.1.4] - 2026-01-25

### Fixed
- Ghostty terminal support (#26)

## [0.1.3] - 2026-01-24

### Fixed
- Kitty keyboard and other terminal queries filtered from history (#23)

### Changed
- README: Ctrl+Shift+6 for lookback on macOS

## [0.1.2] - 2026-01-22

### Fixed
- Kitty keyboard protocol handling (#21)

## [0.1.1] - 2026-01-22

### Added
- CLI, TOML config file (`~/.config/claude-chill.toml`), and configurable lookback key
- CI workflow (#1)
- macOS support (#3)
- VT100 emulator-based rendering (#6)
- Auto-lookback mode (#7) and its `-a` short flag (#8)
- Version and git commit in `--version` (#20)

### Fixed
- Key sequences (#2)
- Exit display cleanup (#4)
- Terminal queries removed from history so the terminal does not answer them on stdin (#10)
- Auto-lookback trigger (#20)

## [0.1.0] - 2026-01-16

### Added
- Initial release: PTY proxy that truncates synchronized output blocks
  (`?2026h` … `?2026l`) to `CHILL_MAX_LINES` and keeps the full output in a
  history buffer, dumped by `Ctrl+Shift+PgUp` (lookback)
