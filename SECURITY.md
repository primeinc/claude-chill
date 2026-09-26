# Security

## Reporting Vulnerabilities

If you discover a security vulnerability, please report it responsibly via
[GitHub Security Advisories](https://github.com/primeinc/claude-chill/security/advisories/new)
rather than opening a public issue.

Include:
- Description of the vulnerability
- Steps to reproduce
- Affected version(s)

You should receive a response within 72 hours.

For a formal STRIDE threat analysis, see [THREAT_MODEL.md](THREAT_MODEL.md).

## Trust Boundaries

claude-chill operates as a PTY proxy between two trust domains:

```
Terminal (trusted) <--> claude-chill <--> Child process (partially trusted)
```

**Terminal (upstream):** The user's terminal emulator. claude-chill trusts it to
render escape sequences correctly. Input from the terminal is user-initiated
and forwarded to the child process without modification (except lookback key
interception).

**Child process (downstream):** The spawned process (typically `claude`).
claude-chill does NOT trust its output for replay purposes. The child may emit
arbitrary escape sequences, including mode-setting commands (mouse tracking,
focus reporting, bracketed paste) that would be dangerous to replay into the
terminal during lookback mode.

## Escape Sequence Filtering

Terminal history uses a **whitelist-based filter** (`history_filter.rs`). Every
escape sequence variant from the `termwiz` parser is explicitly classified as
either safe (whitelist) or unsafe (blacklist). There is no catch-all fallback.

**Whitelisted (safe for replay):**
- Text output (Print, PrintString)
- SGR attributes (colors, bold, underline)
- Cursor movement and positioning
- Edit operations (erase line, erase display)
- Window title setting (OSC 0, 1, 2)
- Hyperlinks (OSC 8)
- Graphics (Sixel, Kitty images)

**Blacklisted (stripped from history):**
- Mode setting (focus tracking, mouse, bracketed paste)
- Device queries (DA, DSR, DECRQM)
- OSC queries (color queries, clipboard)
- Keyboard protocol changes (Kitty push/pop/set)

## Input Handling

- Lookback key sequences are validated at parse time with length capped at 16 bytes
- Configuration is loaded from a user-owned TOML file with graceful fallback to defaults
- On Windows, command-line arguments are quoted using the `CommandLineToArgvW`-compatible
  escaping algorithm to prevent injection via embedded quotes or backslashes
