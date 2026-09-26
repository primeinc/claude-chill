# Threat Model

Structured STRIDE threat analysis for claude-chill, following the
[Microsoft Security Development Lifecycle](https://www.microsoft.com/securityengineering/sdl/practices).

## System Overview

claude-chill is a PTY proxy that sits between a terminal emulator and a child
process (typically Claude Code). It intercepts VT output, performs differential
rendering to reduce flicker, and maintains a scrollback history buffer.

### Data Flow Diagram

```
 +-----------+      stdin       +--------------+      PTY slave     +-------+
 |  Terminal | --------------> | claude-chill  | -----------------> | Child |
 | (trusted) | <-------------- |    (proxy)    | <----------------- | Proc  |
 +-----------+      stdout     +--------------+      PTY master    +-------+
                                     |
                                     v
                              [History Buffer]
                              (in-memory ring)
```

### Trust Boundaries

| Boundary | From | To | Trust Level |
|----------|------|----|-------------|
| TB-1 | Terminal | Proxy (stdin) | Trusted — user-initiated input |
| TB-2 | Proxy | Terminal (stdout) | Trusted — proxy controls rendering |
| TB-3 | Child | Proxy (PTY master read) | **Partially trusted** — arbitrary escape sequences |
| TB-4 | Proxy | Child (PTY master write) | Trusted — forwarded user input |

### Assets

| Asset | Description | Sensitivity |
|-------|-------------|-------------|
| A-1 | Terminal state (modes, cursor, colors) | Medium — incorrect state causes rendering corruption |
| A-2 | History buffer contents | Medium — may contain sensitive terminal output |
| A-3 | User input stream | High — contains keystrokes, credentials |
| A-4 | Terminal emulator security modes | High — mouse tracking, focus reporting, bracketed paste |
| A-5 | Host process integrity | Critical — proxy runs with user privileges |

---

## STRIDE Analysis by Trust Boundary

### TB-3: Child Process -> Proxy (Primary Attack Surface)

This is the critical trust boundary. The child process can emit arbitrary bytes
including crafted escape sequences.

#### S — Spoofing

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| S-1 | Child emits bytes that mimic the lookback key sequence, causing unintended mode toggle | Lookback key matching uses a streaming byte matcher (`sequence_match.rs`) on **stdin only**; child output is never checked against the lookback key | Mitigated |
| S-2 | Child crafts output that appears to be from the proxy (fake status messages) | Proxy does not emit status messages into the terminal stream; lookback mode uses direct screen writes | Mitigated by design |

#### T — Tampering

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| T-1 | Child sends mode-setting sequences that persist after lookback replay, corrupting terminal state | Whitelist-based history filter (`history_filter.rs`) strips all mode-setting sequences. Every `termwiz::Action` variant is explicitly classified — no catch-all fallback. | Mitigated |
| T-2 | Child sends sequences that exploit stateful parser to smuggle blacklisted sequences past the filter | Filter uses `termwiz::Parser` which maintains full VT state machine. Fuzz target `fuzz_history_filter` tests with arbitrary byte streams. | Mitigated |
| T-3 | Child sends partial escape sequence at chunk boundary to split a blacklisted sequence across filter calls | `termwiz::Parser` is stateful and accumulates partial sequences across calls. Filter processes actions (not raw bytes) after parsing. | Mitigated |

#### R — Repudiation

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| R-1 | No audit trail of what the child process output | History buffer retains filtered output; `--verbose` flag enables stderr logging in release builds | Partially mitigated |
| R-2 | No logging of filter decisions (what was stripped) | Debug-level logging available via `RUST_LOG=debug`; not enabled by default | Accepted risk |

#### I — Information Disclosure

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| I-1 | History buffer contains sensitive terminal content (passwords, tokens) visible during lookback | History buffer is in-memory only, never written to disk. Buffer is dropped when process exits. Lookback mode displays to the same terminal the user is already viewing. | Accepted risk (by design) |
| I-2 | Debug log file contains sensitive terminal output | File-based logging is only available in debug builds (`#[cfg(debug_assertions)]`). Release builds log to stderr only. | Mitigated |

#### D — Denial of Service

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| D-1 | Child floods output to exhaust memory in history buffer | History buffer uses a ring buffer (`LineBuffer`) with configurable max lines (default 100K). Old lines are evicted. | Mitigated |
| D-2 | Child sends pathological escape sequences that cause O(n^2) parsing | `termwiz` and `vt100` parsers are well-tested, linear-time state machines. Sync block buffer is capped at 1 MiB (`SYNC_BUFFER_CAPACITY`). | Mitigated |
| D-3 | Child sends rapid mode changes to thrash Kitty protocol tracker | Kitty tracker uses a simple integer counter; mode changes are O(1). | Mitigated |
| D-4 | Child never sends sync end marker, causing unbounded sync buffer growth | Sync buffer is hard-capped at 1 MiB (`SYNC_BUFFER_CAPACITY`). When exceeded without finding `SYNC_END`, the buffer is force-flushed as a completed sync block and the parser exits sync mode. | Mitigated |

#### E — Elevation of Privilege

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| E-1 | Child exploits unsafe code in proxy to achieve arbitrary code execution | All unsafe blocks are FFI wrappers for POSIX/Win32 APIs with documented SAFETY invariants. Fuzz targets cover parser code paths. | Mitigated |
| E-2 | Child sends escape sequences that cause the terminal to execute commands | Terminal emulators are responsible for their own command execution security. Proxy strips OSC queries and device requests that could leak information. | Out of scope (terminal responsibility) |

### TB-1: Terminal -> Proxy (stdin)

#### S — Spoofing

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| S-3 | Attacker injects keystrokes into the terminal | Out of scope — physical/OS-level security | Out of scope |

#### T — Tampering

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| T-4 | Malformed input crashes the proxy | Input is forwarded to child without parsing (except lookback key matching). Sequence matcher uses a bounded rolling buffer (16 bytes max). | Mitigated |

#### D — Denial of Service

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| D-5 | Rapid stdin input causes proxy to fall behind | Proxy processes stdin with higher priority than rendering. Non-blocking I/O prevents stdin backpressure. | Mitigated |

### TB-2: Proxy -> Terminal (stdout)

#### T — Tampering

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| T-5 | Proxy emits malformed escape sequences that corrupt terminal state | VT renderer uses `vt100::Screen::contents_diff()` which produces well-formed sequences. Output is wrapped in sync markers. | Mitigated |

#### I — Information Disclosure

| ID | Threat | Mitigation | Status |
|----|--------|------------|--------|
| I-3 | Proxy leaks internal state to terminal | Proxy only outputs: child output (filtered during lookback), VT diffs, and history dump. No internal state is serialized to stdout. | Mitigated |

---

## Configuration Security

| Threat | Mitigation | Status |
|--------|------------|--------|
| Malicious config file modifies proxy behavior | Config loaded from user-owned `~/.config/claude-chill.toml`. Graceful fallback to defaults on parse error. No code execution from config. | Mitigated |
| Config injection via environment | No config values read from environment variables (except `RUST_LOG` for log level). | Mitigated |
| Command-line argument injection (Windows) | Arguments quoted using `CommandLineToArgvW`-compatible algorithm to prevent quote/backslash injection. | Mitigated |

---

## Windows-Specific Attack Surface

The Windows implementation (`proxy_windows.rs`, `terminal_windows.rs`) has a
distinct attack surface from Unix:

| Area | Threat | Mitigation | Status |
|------|--------|------------|--------|
| ConPTY pipe I/O | ReadFile/WriteFile on anonymous pipes could block indefinitely | Reader thread isolates blocking reads; main loop uses non-blocking polling via `GetNumberOfConsoleInputEvents` | Mitigated |
| Command-line injection | `CreateProcessW` command line could be manipulated via embedded quotes/backslashes | `quote_arg_windows()` implements `CommandLineToArgvW`-compatible quoting with 11 unit tests | Mitigated |
| Console handle lifetime | Raw HANDLE values (`HPCON`, process/thread handles, pipe handles) must be properly closed | `Drop` impl closes all handles; `conpty` set to 0 after early close to prevent double-free | Mitigated |
| Polling loop CPU usage | `Sleep(5)` busy-wait loop consumes more CPU than `poll(2)` on Unix | Acceptable for a user-interactive tool; 5ms sleep limits CPU to ~1-2% | Accepted risk |
| Thread safety of pipe handles | `HANDLE` is `*mut c_void` (not `Send`); transferred to reader thread via `usize` cast | The cast is safe: pipe handles are valid across threads on Windows; `usize` round-trips correctly | Mitigated |
| Ctrl handler thread safety | `ctrl_handler` is called from a separate OS thread | Handler only performs atomic stores (`AtomicBool`), which are safe from any thread | Mitigated |

---

## Residual Risks

| Risk | Severity | Justification for Acceptance |
|------|----------|------------------------------|
| History buffer contains sensitive content | Medium | By design — the buffer shows the same content the user already sees in their terminal. In-memory only, never persisted. |
| New `termwiz` escape sequence variants could be unclassified | Low | The filter uses explicit match arms with no catch-all. Adding a new variant to `termwiz` would cause a compilation error, forcing explicit classification. |
| Debug logging in debug builds can write to arbitrary files | Low | Debug builds are not distributed. The env var (`CLAUDE_CHILL_LOG_FILE`) is only checked in `#[cfg(debug_assertions)]` builds. |

---

## Review Schedule

This threat model should be reviewed when:
- New trust boundaries are added (e.g., network communication, IPC)
- New escape sequence categories are supported
- The `termwiz` or `vt100` dependencies are upgraded to a new major version
- Platform-specific code paths are significantly modified
