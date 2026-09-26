#![no_main]

use claude_chill::key_parser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes as a key binding string.
    // Should never panic — errors are returned as ParseKeyError.
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(key) = key_parser::parse(s) {
            // If parsing succeeds, escape sequence generation should also not panic
            let _ = key.to_escape_sequence();
            let _ = key.to_kitty_sequence();
            // Display roundtrip should not panic
            let _ = format!("{key}");
        }
    }
});
