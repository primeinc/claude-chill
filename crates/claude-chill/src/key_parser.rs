use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseKeyError {
    pub raw: String,
    pub reason: String,
}

impl ParseKeyError {
    pub fn new(raw: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ParseKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot parse {:?} as key: {}", self.raw, self.reason)
    }
}

impl std::error::Error for ParseKeyError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    F(u8),
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombination {
    pub code: KeyCode,
    pub modifiers: Modifiers,
}

impl KeyCombination {
    pub fn to_escape_sequence(&self) -> Vec<u8> {
        key_to_escape_sequence(&self.code, &self.modifiers)
    }

    pub fn to_kitty_sequence(&self) -> Option<Vec<u8>> {
        let codepoint = match &self.code {
            KeyCode::Char(c) => *c as u32,
            KeyCode::Esc => 27,
            KeyCode::Enter => 13,
            KeyCode::Tab => 9,
            KeyCode::Backspace => 127,
            KeyCode::Space => 32,
            _ => return None,
        };

        let modifier = 1
            + if self.modifiers.shift { 1 } else { 0 }
            + if self.modifiers.alt { 2 } else { 0 }
            + if self.modifiers.ctrl { 4 } else { 0 };

        if modifier == 1 {
            Some(format!("\x1b[{codepoint}u").into_bytes())
        } else {
            Some(format!("\x1b[{codepoint};{modifier}u").into_bytes())
        }
    }
}

impl fmt::Display for KeyCombination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            write!(f, "[ctrl]")?;
        }
        if self.modifiers.shift {
            write!(f, "[shift]")?;
        }
        if self.modifiers.alt {
            write!(f, "[alt]")?;
        }
        let key_name = match &self.code {
            KeyCode::Char(c) => format!("[{c}]"),
            KeyCode::F(n) => format!("[f{n}]"),
            KeyCode::Enter => "[enter]".to_string(),
            KeyCode::Esc => "[esc]".to_string(),
            KeyCode::Tab => "[tab]".to_string(),
            KeyCode::Backspace => "[backspace]".to_string(),
            KeyCode::Delete => "[delete]".to_string(),
            KeyCode::Insert => "[insert]".to_string(),
            KeyCode::Home => "[home]".to_string(),
            KeyCode::End => "[end]".to_string(),
            KeyCode::PageUp => "[pageup]".to_string(),
            KeyCode::PageDown => "[pagedown]".to_string(),
            KeyCode::Up => "[up]".to_string(),
            KeyCode::Down => "[down]".to_string(),
            KeyCode::Left => "[left]".to_string(),
            KeyCode::Right => "[right]".to_string(),
            KeyCode::Space => "[space]".to_string(),
        };
        write!(f, "{key_name}")
    }
}

pub fn parse(raw: &str) -> Result<KeyCombination, ParseKeyError> {
    let raw_lower = raw.to_ascii_lowercase();
    let mut modifiers = Modifiers::default();
    let mut key_code: Option<KeyCode> = None;

    let mut i = 0;
    let chars: Vec<char> = raw_lower.chars().collect();

    while i < chars.len() {
        if chars[i] != '[' {
            return Err(ParseKeyError::new(
                raw,
                format!("expected '[' at position {i}"),
            ));
        }

        let start = i + 1;
        let mut end = start;
        while end < chars.len() && chars[end] != ']' {
            end += 1;
        }

        if end >= chars.len() {
            return Err(ParseKeyError::new(raw, "unclosed bracket"));
        }

        let token: String = chars[start..end].iter().collect();
        i = end + 1;

        match token.as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "shift" => modifiers.shift = true,
            "alt" => modifiers.alt = true,
            _ => {
                if key_code.is_some() {
                    return Err(ParseKeyError::new(raw, "multiple key codes specified"));
                }
                key_code = Some(parse_key_code(&token, raw)?);
            }
        }
    }

    match key_code {
        Some(code) => Ok(KeyCombination { code, modifiers }),
        None => Err(ParseKeyError::new(raw, "no key code specified")),
    }
}

fn parse_key_code(token: &str, raw: &str) -> Result<KeyCode, ParseKeyError> {
    let code = match token {
        "[" => KeyCode::Char('['),
        "]" => KeyCode::Char(']'),
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Space,
        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),
        s if s.len() == 1 => KeyCode::Char(s.chars().next().unwrap_or(' ')),
        _ => return Err(ParseKeyError::new(raw, format!("unknown key: {token}"))),
    };
    Ok(code)
}

fn key_to_escape_sequence(code: &KeyCode, modifiers: &Modifiers) -> Vec<u8> {
    let modifier_code = match (modifiers.ctrl, modifiers.shift, modifiers.alt) {
        (false, false, false) => 0,
        _ => 1 + modifiers.shift as u8 + (modifiers.alt as u8 * 2) + (modifiers.ctrl as u8 * 4),
    };

    match code {
        KeyCode::PageUp => modified_key(b"5", modifier_code),
        KeyCode::PageDown => modified_key(b"6", modifier_code),
        KeyCode::Home => {
            if modifier_code == 0 {
                b"\x1b[H".to_vec()
            } else {
                format!("\x1b[1;{modifier_code}H").into_bytes()
            }
        }
        KeyCode::End => {
            if modifier_code == 0 {
                b"\x1b[F".to_vec()
            } else {
                format!("\x1b[1;{modifier_code}F").into_bytes()
            }
        }
        KeyCode::Up => arrow_key(b'A', modifier_code),
        KeyCode::Down => arrow_key(b'B', modifier_code),
        KeyCode::Right => arrow_key(b'C', modifier_code),
        KeyCode::Left => arrow_key(b'D', modifier_code),
        KeyCode::Insert => modified_key(b"2", modifier_code),
        KeyCode::Delete => modified_key(b"3", modifier_code),
        KeyCode::F(n) => function_key(*n, modifier_code),
        KeyCode::Enter => {
            if modifiers.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        KeyCode::Tab => {
            if modifiers.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Backspace => {
            if modifiers.ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Space => {
            if modifiers.ctrl {
                vec![0x00]
            } else {
                b" ".to_vec()
            }
        }
        KeyCode::Char(c) => char_to_escape_sequence(*c, modifiers),
    }
}

fn char_to_escape_sequence(c: char, modifiers: &Modifiers) -> Vec<u8> {
    if modifiers.ctrl {
        let ctrl_byte = match c {
            'a'..='z' => Some((c.to_ascii_uppercase() as u8) - b'A' + 1),
            'A'..='Z' => Some((c as u8) - b'A' + 1),
            '@' => Some(0x00),
            '[' => Some(0x1B),
            '\\' => Some(0x1C),
            ']' => Some(0x1D),
            '^' | '6' => Some(0x1E),
            '_' | '7' => Some(0x1F),
            '2' => Some(0x00),
            '3' => Some(0x1B),
            '4' => Some(0x1C),
            '5' => Some(0x1D),
            '8' => Some(0x7F),
            _ => None,
        };
        if let Some(byte) = ctrl_byte {
            return if modifiers.alt {
                vec![0x1b, byte]
            } else {
                vec![byte]
            };
        }
    }
    if modifiers.alt {
        vec![0x1b, c as u8]
    } else if modifiers.shift {
        vec![c.to_ascii_uppercase() as u8]
    } else {
        vec![c as u8]
    }
}

fn modified_key(base: &[u8], modifier: u8) -> Vec<u8> {
    if modifier == 0 {
        format!("\x1b[{}~", std::str::from_utf8(base).unwrap_or("")).into_bytes()
    } else {
        format!(
            "\x1b[{};{}~",
            std::str::from_utf8(base).unwrap_or(""),
            modifier
        )
        .into_bytes()
    }
}

fn arrow_key(direction: u8, modifier: u8) -> Vec<u8> {
    if modifier == 0 {
        vec![0x1b, b'[', direction]
    } else {
        format!("\x1b[1;{}{}", modifier, direction as char).into_bytes()
    }
}

fn function_key(n: u8, modifier: u8) -> Vec<u8> {
    let code = match n {
        1 => 11,
        2 => 12,
        3 => 13,
        4 => 14,
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return b"\x1b[24~".to_vec(),
    };

    if modifier == 0 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{modifier}~").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_key() {
        let key = parse("[f12]").unwrap();
        assert_eq!(key.code, KeyCode::F(12));
        assert!(!key.modifiers.ctrl);
        assert!(!key.modifiers.shift);
    }

    #[test]
    fn test_parse_ctrl_shift_pageup() {
        let key = parse("[ctrl][shift][pageup]").unwrap();
        assert_eq!(key.code, KeyCode::PageUp);
        assert!(key.modifiers.ctrl);
        assert!(key.modifiers.shift);
        assert!(!key.modifiers.alt);
    }

    #[test]
    fn test_parse_alt_enter() {
        let key = parse("[alt][enter]").unwrap();
        assert_eq!(key.code, KeyCode::Enter);
        assert!(key.modifiers.alt);
    }

    #[test]
    fn test_parse_single_char() {
        let key = parse("[ctrl][c]").unwrap();
        assert_eq!(key.code, KeyCode::Char('c'));
        assert!(key.modifiers.ctrl);
    }

    #[test]
    fn test_escape_sequence_ctrl_shift_pageup() {
        let key = parse("[ctrl][shift][pageup]").unwrap();
        assert_eq!(key.to_escape_sequence(), b"\x1b[5;6~".to_vec());
    }

    #[test]
    fn test_escape_sequence_f12() {
        let key = parse("[f12]").unwrap();
        assert_eq!(key.to_escape_sequence(), b"\x1b[24~".to_vec());
    }

    #[test]
    fn test_escape_sequence_ctrl_c() {
        let key = parse("[ctrl][c]").unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x03]);
    }

    #[test]
    fn test_display() {
        let key = parse("[ctrl][shift][pageup]").unwrap();
        assert_eq!(key.to_string(), "[ctrl][shift][pageup]");
    }

    #[test]
    fn test_case_insensitive() {
        let key = parse("[CTRL][SHIFT][PAGEUP]").unwrap();
        assert_eq!(key.code, KeyCode::PageUp);
        assert!(key.modifiers.ctrl);
        assert!(key.modifiers.shift);
    }

    #[test]
    fn test_error_unclosed_bracket() {
        let result = parse("[ctrl");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_no_key() {
        let result = parse("[ctrl][shift]");
        assert!(result.is_err());
    }

    #[test]
    fn test_ctrl_caret() {
        let key = parse("[ctrl][^]").unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x1E]);
    }

    #[test]
    fn test_ctrl_6_same_as_ctrl_caret() {
        let key = parse("[ctrl][6]").unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x1E]);
    }

    #[test]
    fn test_ctrl_bracket() {
        let key = parse("[ctrl][[]").unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x1B]);
    }

    #[test]
    fn test_ctrl_backslash() {
        let key = parse("[ctrl][\\]").unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x1C]);
    }

    // ====================================================================
    // Kitty keyboard protocol sequence tests
    // ====================================================================

    #[test]
    fn test_kitty_simple_char() {
        let key = parse("[a]").unwrap();
        // 'a' = codepoint 97, no modifiers → \x1b[97u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[97u".to_vec()));
    }

    #[test]
    fn test_kitty_ctrl_char() {
        let key = parse("[ctrl][c]").unwrap();
        // 'c' = codepoint 99, ctrl = modifier 5 → \x1b[99;5u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[99;5u".to_vec()));
    }

    #[test]
    fn test_kitty_ctrl_6_default_lookback() {
        let key = parse("[ctrl][6]").unwrap();
        // '6' = codepoint 54, ctrl = modifier 5 → \x1b[54;5u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[54;5u".to_vec()));
    }

    #[test]
    fn test_kitty_shift_char() {
        let key = parse("[shift][a]").unwrap();
        // 'a' = 97, shift = modifier 2 → \x1b[97;2u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[97;2u".to_vec()));
    }

    #[test]
    fn test_kitty_alt_char() {
        let key = parse("[alt][x]").unwrap();
        // 'x' = 120, alt = modifier 3 → \x1b[120;3u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[120;3u".to_vec()));
    }

    #[test]
    fn test_kitty_ctrl_shift_char() {
        let key = parse("[ctrl][shift][j]").unwrap();
        // 'j' = 106, ctrl+shift = 1+1+4 = modifier 6 → \x1b[106;6u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[106;6u".to_vec()));
    }

    #[test]
    fn test_kitty_all_modifiers() {
        let key = parse("[ctrl][shift][alt][a]").unwrap();
        // ctrl+shift+alt = 1+1+2+4 = modifier 8 → \x1b[97;8u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[97;8u".to_vec()));
    }

    #[test]
    fn test_kitty_enter() {
        let key = parse("[enter]").unwrap();
        // Enter = codepoint 13, no modifiers → \x1b[13u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[13u".to_vec()));
    }

    #[test]
    fn test_kitty_escape() {
        let key = parse("[esc]").unwrap();
        // Esc = codepoint 27, no modifiers → \x1b[27u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[27u".to_vec()));
    }

    #[test]
    fn test_kitty_tab() {
        let key = parse("[tab]").unwrap();
        // Tab = codepoint 9, no modifiers → \x1b[9u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[9u".to_vec()));
    }

    #[test]
    fn test_kitty_space() {
        let key = parse("[space]").unwrap();
        // Space = codepoint 32, no modifiers → \x1b[32u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[32u".to_vec()));
    }

    #[test]
    fn test_kitty_backspace() {
        let key = parse("[backspace]").unwrap();
        // Backspace = codepoint 127, no modifiers → \x1b[127u
        assert_eq!(key.to_kitty_sequence(), Some(b"\x1b[127u".to_vec()));
    }

    #[test]
    fn test_kitty_f_key_unsupported() {
        // Function keys don't have Kitty codepoints in the current implementation
        let key = parse("[f12]").unwrap();
        assert_eq!(key.to_kitty_sequence(), None);
    }

    #[test]
    fn test_kitty_arrow_unsupported() {
        let key = parse("[up]").unwrap();
        assert_eq!(key.to_kitty_sequence(), None);
    }

    #[test]
    fn test_kitty_pageup_unsupported() {
        let key = parse("[pageup]").unwrap();
        assert_eq!(key.to_kitty_sequence(), None);
    }

    // ====================================================================
    // Error handling and edge case tests
    // ====================================================================

    #[test]
    fn test_error_empty_input() {
        let result = parse("");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_no_brackets() {
        let result = parse("ctrl+c");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_missing_closing_bracket() {
        let result = parse("[ctrl][a");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_multiple_key_codes() {
        let result = parse("[a][b]");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unknown_key_name() {
        let result = parse("[ctrl][superduperkey]");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_only_modifiers() {
        // ctrl+shift with no actual key
        let result = parse("[ctrl][shift]");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_error_display() {
        let err = parse("[ctrl][badkey]").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("badkey"), "error should mention the bad key");
        assert!(
            msg.contains("[ctrl][badkey]"),
            "error should include the raw input"
        );
    }

    #[test]
    fn test_parse_key_aliases() {
        // Test all documented key name aliases
        assert!(parse("[esc]").is_ok());
        assert!(parse("[escape]").is_ok());
        assert!(parse("[enter]").is_ok());
        assert!(parse("[return]").is_ok());
        assert!(parse("[bs]").is_ok());
        assert!(parse("[backspace]").is_ok());
        assert!(parse("[del]").is_ok());
        assert!(parse("[delete]").is_ok());
        assert!(parse("[ins]").is_ok());
        assert!(parse("[insert]").is_ok());
        assert!(parse("[pgup]").is_ok());
        assert!(parse("[pageup]").is_ok());
        assert!(parse("[pgdn]").is_ok());
        assert!(parse("[pgdown]").is_ok());
        assert!(parse("[pagedown]").is_ok());
    }

    #[test]
    fn test_parse_control_alias() {
        let key = parse("[control][a]").unwrap();
        assert!(key.modifiers.ctrl);
        assert_eq!(key.code, KeyCode::Char('a'));
    }

    #[test]
    fn test_escape_sequence_all_function_keys() {
        for n in 1..=12 {
            let key = parse(&format!("[f{n}]")).unwrap();
            assert_eq!(key.code, KeyCode::F(n));
            let seq = key.to_escape_sequence();
            // All function keys should produce escape sequences starting with ESC [
            assert!(seq.starts_with(b"\x1b["), "f{n} should start with ESC [");
        }
    }

    #[test]
    fn test_escape_sequence_arrows() {
        let key_up = parse("[up]").unwrap();
        assert_eq!(key_up.to_escape_sequence(), b"\x1b[A".to_vec());
        let key_down = parse("[down]").unwrap();
        assert_eq!(key_down.to_escape_sequence(), b"\x1b[B".to_vec());
        let key_right = parse("[right]").unwrap();
        assert_eq!(key_right.to_escape_sequence(), b"\x1b[C".to_vec());
        let key_left = parse("[left]").unwrap();
        assert_eq!(key_left.to_escape_sequence(), b"\x1b[D".to_vec());
    }

    #[test]
    fn test_display_roundtrip() {
        // Display output should be parseable back to the same key
        let original = parse("[ctrl][shift][f5]").unwrap();
        let displayed = original.to_string();
        let reparsed = parse(&displayed).unwrap();
        assert_eq!(original.code, reparsed.code);
        assert_eq!(original.modifiers, reparsed.modifiers);
    }

    #[test]
    fn test_display_roundtrip_all_special_keys() {
        // Verify parse → Display → parse roundtrip for all special keys
        let keys = [
            "[enter]",
            "[esc]",
            "[tab]",
            "[backspace]",
            "[delete]",
            "[insert]",
            "[home]",
            "[end]",
            "[pageup]",
            "[pagedown]",
            "[up]",
            "[down]",
            "[left]",
            "[right]",
            "[space]",
            "[f1]",
            "[f6]",
            "[f12]",
            "[a]",
            "[z]",
            "[0]",
            "[9]",
        ];
        for key_str in &keys {
            let original = parse(key_str).unwrap();
            let displayed = original.to_string();
            let reparsed = parse(&displayed).unwrap_or_else(|e| {
                panic!("roundtrip failed for {key_str}: displayed as '{displayed}', error: {e}")
            });
            assert_eq!(original.code, reparsed.code, "code mismatch for {key_str}");
            assert_eq!(
                original.modifiers, reparsed.modifiers,
                "modifier mismatch for {key_str}"
            );
        }
    }

    #[test]
    fn test_display_roundtrip_with_modifiers() {
        let combos = [
            "[ctrl][a]",
            "[shift][a]",
            "[alt][a]",
            "[ctrl][shift][a]",
            "[ctrl][alt][a]",
            "[shift][alt][a]",
            "[ctrl][shift][alt][a]",
            "[ctrl][f1]",
            "[alt][enter]",
            "[shift][tab]",
            "[ctrl][space]",
        ];
        for key_str in &combos {
            let original = parse(key_str).unwrap();
            let displayed = original.to_string();
            let reparsed = parse(&displayed).unwrap_or_else(|e| {
                panic!("roundtrip failed for {key_str}: displayed as '{displayed}', error: {e}")
            });
            assert_eq!(original.code, reparsed.code, "code mismatch for {key_str}");
            assert_eq!(
                original.modifiers, reparsed.modifiers,
                "modifier mismatch for {key_str}"
            );
        }
    }
}
