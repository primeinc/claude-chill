//! Whitelist-based filter for terminal escape sequences going into history.
//!
//! Uses termwiz to parse escape sequences and only allows known-safe
//! visual sequences through. This prevents mode-setting sequences
//! (focus reporting, mouse tracking, etc.) from being replayed.
//!
//! IMPORTANT: Every enum variant must be explicitly classified.
//! No catch-all fallbacks - we must consciously decide on each case.

use std::borrow::Cow;
use std::fmt::Write as FmtWrite;
use termwiz::escape::Action;
use termwiz::escape::csi::CSI;
use termwiz::escape::parser::Parser;

/// Filter that only allows safe visual sequences into history.
pub struct HistoryFilter {
    parser: Parser,
}

impl Default for HistoryFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryFilter {
    /// Create a new filter with a fresh termwiz parser.
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
        }
    }

    /// Filter bytes, returning only safe sequences for history.
    ///
    /// If all parsed actions are safe (common case), returns a borrowed reference
    /// to the input bytes directly, avoiding allocation and preserving byte-level
    /// fidelity.
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        let actions = self.parser.parse_as_vec(input);

        // Fast path: if everything is safe, return input bytes directly.
        // This avoids the Display re-encoding which can alter byte-level output
        // (e.g. combined SGR params, elided default cursor params) and skips
        // the Vec allocation entirely.
        if actions.iter().all(is_safe_for_history) {
            return Cow::Borrowed(input);
        }

        // Slow path: some actions are blacklisted, re-encode only the safe ones.
        let mut output = String::with_capacity(input.len());
        for action in actions {
            if is_safe_for_history(&action) {
                let _ = write!(output, "{action}");
            }
        }
        Cow::Owned(output.into_bytes())
    }
}

/// Classification result - explicit whitelist or blacklist
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    /// Safe for history - purely visual, no side effects
    Whitelist,
    /// Unsafe for history - triggers responses or enables modes
    Blacklist,
}

/// Check if an action is safe to include in history.
fn is_safe_for_history(action: &Action) -> bool {
    classify_action(action) == Classification::Whitelist
}

/// Classify an action as whitelist or blacklist.
/// Every variant must be explicitly handled - no wildcards.
fn classify_action(action: &Action) -> Classification {
    use Classification::*;

    match action {
        // === WHITELIST: Text output ===
        Action::Print(_) => Whitelist,
        Action::PrintString(_) => Whitelist,

        // === WHITELIST: Graphics data ===
        Action::Sixel(_) => Whitelist,
        Action::KittyImage(_) => Whitelist,

        // === MIXED: CSI sequences - check subcategory ===
        Action::CSI(csi) => classify_csi(csi),

        // === MIXED: Control codes - check individually ===
        Action::Control(code) => classify_control(*code),

        // === MIXED: Escape sequences - check individually ===
        Action::Esc(esc) => classify_esc(esc),

        // === MIXED: OSC commands - check for queries ===
        Action::OperatingSystemCommand(osc) => classify_osc(osc),

        // === BLACKLIST: Device control (DCS) - queries and modes ===
        Action::DeviceControl(_) => Blacklist,

        // === BLACKLIST: Termcap query ===
        Action::XtGetTcap(_) => Blacklist,
    }
}

/// Classify CSI sequences - every variant explicitly handled.
fn classify_csi(csi: &CSI) -> Classification {
    use Classification::*;

    match csi {
        // === WHITELIST: SGR (colors, styling) ===
        CSI::Sgr(_) => Whitelist,

        // === WHITELIST: Cursor movement ===
        CSI::Cursor(_) => Whitelist,

        // === WHITELIST: Edit operations ===
        CSI::Edit(_) => Whitelist,

        // === WHITELIST: Character path (bidi) ===
        CSI::SelectCharacterPath(_, _) => Whitelist,

        // === BLACKLIST: Mode setting (DECSET/DECRST) ===
        // Includes focus tracking, mouse modes, bracketed paste, etc.
        CSI::Mode(_) => Blacklist,

        // === BLACKLIST: Device queries ===
        CSI::Device(_) => Blacklist,

        // === BLACKLIST: Keyboard protocol ===
        CSI::Keyboard(_) => Blacklist,

        // === BLACKLIST: Mouse reports ===
        // These come IN from terminal, shouldn't be in output anyway
        CSI::Mouse(_) => Blacklist,

        // === MIXED: Window operations ===
        CSI::Window(w) => classify_window(w),

        // === BLACKLIST: Unspecified/unknown ===
        // We can't know if it's safe, so blacklist
        CSI::Unspecified(_) => Blacklist,
    }
}

/// Classify window operations - every variant explicitly handled.
fn classify_window(window: &termwiz::escape::csi::Window) -> Classification {
    use Classification::*;
    use termwiz::escape::csi::Window;

    match window {
        // === WHITELIST: Window state modifications ===
        Window::DeIconify => Whitelist,
        Window::Iconify => Whitelist,
        Window::MoveWindow { .. } => Whitelist,
        Window::ResizeWindowPixels { .. } => Whitelist,
        Window::RaiseWindow => Whitelist,
        Window::LowerWindow => Whitelist,
        Window::RefreshWindow => Whitelist,
        Window::ResizeWindowCells { .. } => Whitelist,
        Window::RestoreMaximizedWindow => Whitelist,
        Window::MaximizeWindow => Whitelist,
        Window::MaximizeWindowVertically => Whitelist,
        Window::MaximizeWindowHorizontally => Whitelist,
        Window::UndoFullScreenMode => Whitelist,
        Window::ChangeToFullScreenMode => Whitelist,
        Window::ToggleFullScreen => Whitelist,

        // === WHITELIST: Title stack operations ===
        Window::PushIconAndWindowTitle => Whitelist,
        Window::PushIconTitle => Whitelist,
        Window::PushWindowTitle => Whitelist,
        Window::PopIconAndWindowTitle => Whitelist,
        Window::PopIconTitle => Whitelist,
        Window::PopWindowTitle => Whitelist,

        // === BLACKLIST: Query operations (terminal sends response) ===
        Window::ReportWindowState => Blacklist,
        Window::ReportWindowPosition => Blacklist,
        Window::ReportTextAreaPosition => Blacklist,
        Window::ReportTextAreaSizePixels => Blacklist,
        Window::ReportWindowSizePixels => Blacklist,
        Window::ReportScreenSizePixels => Blacklist,
        Window::ReportCellSizePixels => Blacklist,
        Window::ReportCellSizePixelsResponse { .. } => Blacklist,
        Window::ReportTextAreaSizeCells => Blacklist,
        Window::ReportScreenSizeCells => Blacklist,
        Window::ReportIconLabel => Blacklist,
        Window::ReportWindowTitle => Blacklist,
        Window::ChecksumRectangularArea { .. } => Blacklist,
    }
}

/// Classify control codes - every variant explicitly handled.
fn classify_control(code: termwiz::escape::ControlCode) -> Classification {
    use Classification::*;
    use termwiz::escape::ControlCode;

    match code {
        // === WHITELIST: Common formatting controls ===
        ControlCode::Null => Whitelist,
        ControlCode::Bell => Whitelist,
        ControlCode::Backspace => Whitelist,
        ControlCode::HorizontalTab => Whitelist,
        ControlCode::LineFeed => Whitelist,
        ControlCode::VerticalTab => Whitelist,
        ControlCode::FormFeed => Whitelist,
        ControlCode::CarriageReturn => Whitelist,
        ControlCode::ShiftOut => Whitelist,
        ControlCode::ShiftIn => Whitelist,

        // === BLACKLIST: ENQ triggers terminal response ===
        ControlCode::Enquiry => Blacklist,

        // === BLACKLIST: Other C0 controls ===
        ControlCode::StartOfHeading => Blacklist,
        ControlCode::StartOfText => Blacklist,
        ControlCode::EndOfText => Blacklist,
        ControlCode::EndOfTransmission => Blacklist,
        ControlCode::Acknowledge => Blacklist,
        ControlCode::DataLinkEscape => Blacklist,
        ControlCode::DeviceControlOne => Blacklist,
        ControlCode::DeviceControlTwo => Blacklist,
        ControlCode::DeviceControlThree => Blacklist,
        ControlCode::DeviceControlFour => Blacklist,
        ControlCode::NegativeAcknowledge => Blacklist,
        ControlCode::SynchronousIdle => Blacklist,
        ControlCode::EndOfTransmissionBlock => Blacklist,
        ControlCode::Cancel => Blacklist,
        ControlCode::EndOfMedium => Blacklist,
        ControlCode::Substitute => Blacklist,
        ControlCode::Escape => Blacklist, // ESC alone shouldn't appear
        ControlCode::FileSeparator => Blacklist,
        ControlCode::GroupSeparator => Blacklist,
        ControlCode::RecordSeparator => Blacklist,
        ControlCode::UnitSeparator => Blacklist,

        // === BLACKLIST: C1 8-bit controls ===
        ControlCode::BPH => Blacklist,
        ControlCode::NBH => Blacklist,
        ControlCode::IND => Blacklist, // Use ESC D instead
        ControlCode::NEL => Blacklist, // Use ESC E instead
        ControlCode::SSA => Blacklist,
        ControlCode::ESA => Blacklist,
        ControlCode::HTS => Blacklist, // Use ESC H instead
        ControlCode::HTJ => Blacklist,
        ControlCode::VTS => Blacklist,
        ControlCode::PLD => Blacklist,
        ControlCode::PLU => Blacklist,
        ControlCode::RI => Blacklist, // Use ESC M instead
        ControlCode::SS2 => Blacklist,
        ControlCode::SS3 => Blacklist,
        ControlCode::DCS => Blacklist, // Device control string
        ControlCode::PU1 => Blacklist,
        ControlCode::PU2 => Blacklist,
        ControlCode::STS => Blacklist,
        ControlCode::CCH => Blacklist,
        ControlCode::MW => Blacklist,
        ControlCode::SPA => Blacklist,
        ControlCode::EPA => Blacklist,
        ControlCode::SOS => Blacklist,
        ControlCode::SCI => Blacklist,
        ControlCode::CSI => Blacklist, // CSI alone shouldn't appear
        ControlCode::ST => Blacklist,  // String terminator
        ControlCode::OSC => Blacklist, // OSC alone shouldn't appear
        ControlCode::PM => Blacklist,
        ControlCode::APC => Blacklist,
    }
}

/// Classify escape sequences - every variant explicitly handled.
fn classify_esc(esc: &termwiz::escape::Esc) -> Classification {
    use Classification::*;
    use termwiz::escape::{Esc, EscCode};

    match esc {
        Esc::Code(code) => match code {
            // === WHITELIST: Cursor save/restore ===
            EscCode::DecSaveCursorPosition => Whitelist,
            EscCode::DecRestoreCursorPosition => Whitelist,

            // === WHITELIST: Character set designation (G0) ===
            EscCode::DecLineDrawingG0 => Whitelist,
            EscCode::AsciiCharacterSetG0 => Whitelist,
            EscCode::UkCharacterSetG0 => Whitelist,

            // === WHITELIST: Character set designation (G1) ===
            EscCode::DecLineDrawingG1 => Whitelist,
            EscCode::AsciiCharacterSetG1 => Whitelist,
            EscCode::UkCharacterSetG1 => Whitelist,

            // === WHITELIST: Line operations ===
            EscCode::Index => Whitelist,
            EscCode::ReverseIndex => Whitelist,
            EscCode::NextLine => Whitelist,

            // === WHITELIST: Tab set ===
            EscCode::HorizontalTabSet => Whitelist,

            // === WHITELIST: Keypad modes ===
            EscCode::DecApplicationKeyPad => Whitelist,
            EscCode::DecNormalKeyPad => Whitelist,

            // === WHITELIST: String terminator ===
            EscCode::StringTerminator => Whitelist,

            // === WHITELIST: Full reset ===
            EscCode::FullReset => Whitelist,

            // === WHITELIST: Tmux title ===
            EscCode::TmuxTitle => Whitelist,

            // === WHITELIST: Cursor position ===
            EscCode::CursorPositionLowerLeft => Whitelist,

            // === BLACKLIST: Single shifts ===
            EscCode::SingleShiftG2 => Blacklist,
            EscCode::SingleShiftG3 => Blacklist,

            // === BLACKLIST: Guarded area ===
            EscCode::StartOfGuardedArea => Blacklist,
            EscCode::EndOfGuardedArea => Blacklist,

            // === BLACKLIST: Start of string ===
            EscCode::StartOfString => Blacklist,

            // === BLACKLIST: Return terminal ID (query) ===
            EscCode::ReturnTerminalId => Blacklist,

            // === BLACKLIST: Privacy message ===
            EscCode::PrivacyMessage => Blacklist,

            // === BLACKLIST: Application program command ===
            EscCode::ApplicationProgramCommand => Blacklist,

            // === BLACKLIST: Back index ===
            EscCode::DecBackIndex => Blacklist,

            // === BLACKLIST: DEC line width/height ===
            EscCode::DecDoubleHeightTopHalfLine => Blacklist,
            EscCode::DecDoubleHeightBottomHalfLine => Blacklist,
            EscCode::DecSingleWidthLine => Blacklist,
            EscCode::DecDoubleWidthLine => Blacklist,
            EscCode::DecScreenAlignmentDisplay => Blacklist,

            // === BLACKLIST: Application mode key presses (input, not output) ===
            EscCode::ApplicationModeArrowUpPress => Blacklist,
            EscCode::ApplicationModeArrowDownPress => Blacklist,
            EscCode::ApplicationModeArrowRightPress => Blacklist,
            EscCode::ApplicationModeArrowLeftPress => Blacklist,
            EscCode::ApplicationModeHomePress => Blacklist,
            EscCode::ApplicationModeEndPress => Blacklist,
            EscCode::F1Press => Blacklist,
            EscCode::F2Press => Blacklist,
            EscCode::F3Press => Blacklist,
            EscCode::F4Press => Blacklist,
        },

        // === BLACKLIST: Unspecified escape sequences ===
        Esc::Unspecified { .. } => Blacklist,
    }
}

/// Classify OSC commands - every variant explicitly handled.
fn classify_osc(osc: &termwiz::escape::OperatingSystemCommand) -> Classification {
    use Classification::*;
    use termwiz::escape::osc::OperatingSystemCommand;

    match osc {
        // === WHITELIST: Title setting ===
        OperatingSystemCommand::SetIconNameAndWindowTitle(_) => Whitelist,
        OperatingSystemCommand::SetIconName(_) => Whitelist,
        OperatingSystemCommand::SetIconNameSun(_) => Whitelist,
        OperatingSystemCommand::SetWindowTitle(_) => Whitelist,
        OperatingSystemCommand::SetWindowTitleSun(_) => Whitelist,

        // === WHITELIST: Hyperlinks ===
        OperatingSystemCommand::SetHyperlink(_) => Whitelist,

        // === WHITELIST: Color palette setting ===
        OperatingSystemCommand::ChangeColorNumber(_) => Whitelist,

        // === WHITELIST: Reset colors ===
        OperatingSystemCommand::ResetColors(_) => Whitelist,

        // === WHITELIST: Reset dynamic color ===
        OperatingSystemCommand::ResetDynamicColor(_) => Whitelist,

        // === WHITELIST: Selection setting (not query) ===
        OperatingSystemCommand::SetSelection(_, _) => Whitelist,

        // === WHITELIST: System notification ===
        OperatingSystemCommand::SystemNotification(_) => Whitelist,

        // === BLACKLIST: Selection clear ===
        OperatingSystemCommand::ClearSelection(_) => Blacklist,

        // === BLACKLIST: Selection query ===
        OperatingSystemCommand::QuerySelection(_) => Blacklist,

        // === MIXED: Dynamic colors (check for query) ===
        OperatingSystemCommand::ChangeDynamicColors(_, colors) => {
            // Whitelist only if ALL are Color, not Query
            if colors
                .iter()
                .all(|c| matches!(c, termwiz::escape::osc::ColorOrQuery::Color(_)))
            {
                Whitelist
            } else {
                Blacklist
            }
        }

        // === MIXED: iTerm proprietary ===
        OperatingSystemCommand::ITermProprietary(iterm) => classify_iterm(iterm),

        // === WHITELIST: FinalTerm semantic prompts ===
        OperatingSystemCommand::FinalTermSemanticPrompt(_) => Whitelist,

        // === BLACKLIST: Current directory (pwd) - could leak info ===
        OperatingSystemCommand::CurrentWorkingDirectory(_) => Blacklist,

        // === BLACKLIST: Rxvt extension ===
        OperatingSystemCommand::RxvtExtension(_) => Blacklist,

        // === BLACKLIST: ConEmu progress ===
        OperatingSystemCommand::ConEmuProgress(_) => Blacklist,

        // === BLACKLIST: Unspecified OSC ===
        OperatingSystemCommand::Unspecified(_) => Blacklist,
    }
}

/// Classify iTerm proprietary OSC commands.
fn classify_iterm(iterm: &termwiz::escape::osc::ITermProprietary) -> Classification {
    use Classification::*;
    use termwiz::escape::osc::ITermProprietary;

    match iterm {
        // === WHITELIST: File/image data ===
        ITermProprietary::File(_) => Whitelist,

        // === WHITELIST: Marks ===
        ITermProprietary::SetMark => Whitelist,

        // === WHITELIST: User variables ===
        ITermProprietary::SetUserVar { .. } => Whitelist,

        // === WHITELIST: Badge ===
        ITermProprietary::SetBadgeFormat(_) => Whitelist,

        // === WHITELIST: Profile ===
        ITermProprietary::SetProfile(_) => Whitelist,

        // === WHITELIST: Copy to pasteboard ===
        ITermProprietary::CopyToClipboard(_) => Whitelist,

        // === WHITELIST: Copy string ===
        ITermProprietary::Copy(_) => Whitelist,

        // === BLACKLIST: Current directory ===
        ITermProprietary::CurrentDir(_) => Blacklist,

        // === BLACKLIST: Cell size request (triggers response) ===
        ITermProprietary::RequestCellSize => Blacklist,

        // === BLACKLIST: Cell size response ===
        ITermProprietary::ReportCellSize { .. } => Blacklist,

        // === BLACKLIST: Report variable (triggers response) ===
        ITermProprietary::ReportVariable(_) => Blacklist,

        // === BLACKLIST: Unicode version ===
        ITermProprietary::UnicodeVersion(_) => Blacklist,

        // === BLACKLIST: Stealing focus ===
        ITermProprietary::StealFocus => Blacklist,

        // === BLACKLIST: Clear scrollback ===
        ITermProprietary::ClearScrollback => Blacklist,

        // === BLACKLIST: End copy ===
        ITermProprietary::EndCopy => Blacklist,

        // === BLACKLIST: Highlight cursor line ===
        ITermProprietary::HighlightCursorLine(_) => Blacklist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_passes() {
        let mut filter = HistoryFilter::new();
        let output = filter.filter(b"Hello, World!");
        assert_eq!(output.as_ref(), b"Hello, World!");
    }

    #[test]
    fn test_sgr_passes() {
        let mut filter = HistoryFilter::new();
        // Bold red text
        let input = b"\x1b[1;31mRed\x1b[0m";
        let output = filter.filter(input);
        // termwiz re-encodes, so check it contains the key parts
        assert!(!output.is_empty());
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("Red"));
    }

    #[test]
    fn test_cursor_movement_passes() {
        let mut filter = HistoryFilter::new();
        // Move cursor to position 1,1
        let input = b"\x1b[1;1H";
        let output = filter.filter(input);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_focus_tracking_filtered() {
        let mut filter = HistoryFilter::new();
        // Enable focus reporting (CSI ? 1004 h)
        let input = b"\x1b[?1004h";
        let output = filter.filter(input);
        assert!(output.is_empty(), "Focus tracking should be filtered");
    }

    #[test]
    fn test_mouse_mode_filtered() {
        let mut filter = HistoryFilter::new();
        // Enable mouse tracking (CSI ? 1000 h)
        let input = b"\x1b[?1000h";
        let output = filter.filter(input);
        assert!(output.is_empty(), "Mouse tracking should be filtered");
    }

    #[test]
    fn test_bracketed_paste_filtered() {
        let mut filter = HistoryFilter::new();
        // Enable bracketed paste (CSI ? 2004 h)
        let input = b"\x1b[?2004h";
        let output = filter.filter(input);
        assert!(output.is_empty(), "Bracketed paste should be filtered");
    }

    #[test]
    fn test_device_attributes_filtered() {
        let mut filter = HistoryFilter::new();
        // Primary DA query
        let input = b"\x1b[c";
        let output = filter.filter(input);
        assert!(output.is_empty(), "DA query should be filtered");
    }

    #[test]
    fn test_mixed_content() {
        let mut filter = HistoryFilter::new();
        // Mix of safe and unsafe
        let input = b"Hello\x1b[?1004hWorld\x1b[31mRed\x1b[0m";
        let output = filter.filter(input);
        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("Hello"));
        assert!(output_str.contains("World"));
        assert!(output_str.contains("Red"));
        // Should not contain the focus tracking sequence
        assert!(!output_str.contains("1004"));
    }

    #[test]
    fn test_osc_title_passes() {
        let mut filter = HistoryFilter::new();
        // Set window title
        let input = b"\x1b]0;My Title\x07";
        let output = filter.filter(input);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_osc_query_filtered() {
        let mut filter = HistoryFilter::new();
        // Query foreground color
        let input = b"\x1b]10;?\x07";
        let output = filter.filter(input);
        assert!(output.is_empty(), "OSC query should be filtered");
    }

    // ====================================================================
    // Round-trip fidelity tests: verify that the parse→classify→re-encode
    // pipeline produces byte-equivalent output for common escape sequences.
    // ====================================================================

    /// Helper: assert that filtering produces byte-identical output.
    fn assert_roundtrip_exact(input: &[u8], label: &str) {
        let mut filter = HistoryFilter::new();
        let output = filter.filter(input);
        assert_eq!(
            output, input,
            "Round-trip fidelity failed for {label}: input={input:?} output={output:?}"
        );
    }

    /// Helper: assert that filtering produces output that a VT100 parser
    /// renders to the same screen contents as the input.
    fn assert_roundtrip_visual(input: &[u8], label: &str) {
        let mut filter = HistoryFilter::new();
        let output = filter.filter(input);

        let mut parser_in = vt100::Parser::new(24, 80, 0);
        parser_in.process(input);
        let mut parser_out = vt100::Parser::new(24, 80, 0);
        parser_out.process(&output);

        let screen_in = parser_in.screen().contents();
        let screen_out = parser_out.screen().contents();
        assert_eq!(screen_in, screen_out, "Visual fidelity failed for {label}");
    }

    #[test]
    fn test_roundtrip_plain_text() {
        assert_roundtrip_exact(b"Hello, World!", "plain text");
    }

    #[test]
    fn test_roundtrip_newlines() {
        assert_roundtrip_exact(b"line1\r\nline2\r\n", "CRLF text");
    }

    #[test]
    fn test_roundtrip_clear_screen() {
        assert_roundtrip_exact(b"\x1b[2J", "clear screen");
    }

    #[test]
    fn test_roundtrip_cursor_home() {
        assert_roundtrip_exact(b"\x1b[H", "cursor home");
    }

    #[test]
    fn test_roundtrip_erase_line() {
        assert_roundtrip_exact(b"\x1b[K", "erase to end of line");
    }

    #[test]
    fn test_roundtrip_sgr_reset() {
        assert_roundtrip_exact(b"\x1b[0m", "SGR reset");
    }

    #[test]
    fn test_roundtrip_cursor_movement() {
        // Cursor to specific position
        assert_roundtrip_exact(b"\x1b[10;20H", "cursor position");
    }

    #[test]
    fn test_roundtrip_cursor_up() {
        assert_roundtrip_exact(b"\x1b[5A", "cursor up 5");
    }

    #[test]
    fn test_roundtrip_scroll_up() {
        assert_roundtrip_exact(b"\x1b[3S", "scroll up 3");
    }

    #[test]
    fn test_roundtrip_insert_lines() {
        assert_roundtrip_exact(b"\x1b[2L", "insert 2 lines");
    }

    #[test]
    fn test_roundtrip_sgr_visual_fidelity() {
        // SGR may re-encode (e.g., bold+red), so test visual equivalence
        assert_roundtrip_visual(b"\x1b[1;31mBold Red\x1b[0m Normal", "bold red SGR");
    }

    #[test]
    fn test_roundtrip_256_color_visual_fidelity() {
        assert_roundtrip_visual(b"\x1b[38;5;196mRed256\x1b[0m", "256-color SGR");
    }

    #[test]
    fn test_roundtrip_truecolor_visual_fidelity() {
        assert_roundtrip_visual(b"\x1b[38;2;255;128;0mOrange\x1b[0m", "truecolor SGR");
    }

    #[test]
    fn test_roundtrip_mixed_visual_fidelity() {
        // A realistic Claude Code output snippet: cursor positioning + SGR + text
        let input = b"\x1b[H\x1b[2J\x1b[1;1H\x1b[1;34mFile:\x1b[0m src/main.rs\r\n\x1b[32m+ added line\x1b[0m\r\n";
        assert_roundtrip_visual(input, "mixed Claude-like output");
    }

    #[test]
    fn test_roundtrip_dec_line_drawing() {
        // DEC special graphics mode (box drawing)
        assert_roundtrip_exact(b"\x1b(0", "DEC line drawing G0");
        assert_roundtrip_exact(b"\x1b(B", "ASCII charset G0");
    }

    #[test]
    fn test_roundtrip_save_restore_cursor() {
        assert_roundtrip_exact(b"\x1b7", "save cursor");
        assert_roundtrip_exact(b"\x1b8", "restore cursor");
    }

    #[test]
    fn test_roundtrip_tab_and_newline() {
        assert_roundtrip_exact(b"\t", "tab");
        assert_roundtrip_exact(b"\n", "newline");
        assert_roundtrip_exact(b"\r", "carriage return");
    }

    // ====================================================================
    // Known re-encoding divergences: termwiz Display may emit bytes that
    // differ from the original input. These tests document the divergences
    // and verify visual equivalence despite byte-level differences.
    // ====================================================================

    #[test]
    fn test_reencoding_sgr_combined_params() {
        // \x1b[1;31m (bold + red) — termwiz may split into separate SGR sequences
        let mut filter = HistoryFilter::new();
        let input = b"\x1b[1;31mtext\x1b[0m";
        let output = filter.filter(input);

        // Verify visual equivalence even if bytes differ
        let mut p_in = vt100::Parser::new(24, 80, 0);
        p_in.process(input);
        let mut p_out = vt100::Parser::new(24, 80, 0);
        p_out.process(&output);
        assert_eq!(
            p_in.screen().contents(),
            p_out.screen().contents(),
            "combined SGR must be visually equivalent"
        );

        // Document whether this particular case round-trips exactly
        if output != input.to_vec() {
            // Known divergence: termwiz may re-encode combined SGR differently.
            // This is acceptable as long as visual output is identical.
        }
    }

    #[test]
    fn test_reencoding_cursor_default_params() {
        // \x1b[1;1H — termwiz may normalize to \x1b[H (default params elided)
        let mut filter = HistoryFilter::new();
        let input = b"\x1b[1;1H";
        let output = filter.filter(input);

        let mut p_in = vt100::Parser::new(24, 80, 0);
        p_in.process(input);
        let mut p_out = vt100::Parser::new(24, 80, 0);
        p_out.process(&output);
        assert_eq!(
            p_in.screen().cursor_position(),
            p_out.screen().cursor_position(),
            "cursor position must match"
        );
    }

    #[test]
    fn test_reencoding_sgr_reset() {
        // \x1b[0m — termwiz may re-encode as \x1b[m (eliding the 0)
        let mut filter = HistoryFilter::new();
        let input = b"\x1b[0m";
        let output = filter.filter(input);

        // Both forms are visually identical
        let mut p_in = vt100::Parser::new(24, 80, 0);
        p_in.process(b"\x1b[1;31mRed\x1b[0m Normal");
        let mut p_out = vt100::Parser::new(24, 80, 0);
        p_out.process(b"\x1b[1;31mRed");
        p_out.process(&output);
        p_out.process(b" Normal");
        // Both should have "Normal" without attributes
        assert_eq!(
            p_in.screen().contents(),
            p_out.screen().contents(),
            "SGR reset must produce same visual result"
        );
    }

    // ====================================================================
    // Fast-path tests: verify that all-safe input is returned byte-exactly
    // ====================================================================

    #[test]
    fn test_fast_path_plain_text() {
        let mut filter = HistoryFilter::new();
        let input = b"Hello, World!";
        let output = filter.filter(input);
        assert_eq!(
            output.as_ref(),
            input,
            "fast path: plain text should return exact bytes"
        );
    }

    #[test]
    fn test_fast_path_combined_sgr() {
        // Combined SGR was previously re-encoded differently by Display.
        // The fast path should now return the original bytes.
        let mut filter = HistoryFilter::new();
        let input = b"\x1b[1;31mBold Red\x1b[0m";
        let output = filter.filter(input);
        assert_eq!(
            output,
            input.to_vec(),
            "fast path: combined SGR should return exact bytes"
        );
    }

    #[test]
    fn test_fast_path_cursor_with_defaults() {
        let mut filter = HistoryFilter::new();
        let input = b"\x1b[1;1H";
        let output = filter.filter(input);
        assert_eq!(
            output,
            input.to_vec(),
            "fast path: cursor position should return exact bytes"
        );
    }

    #[test]
    fn test_slow_path_mixed_content() {
        // When blacklisted content is present, the slow path is used.
        // Verify it still works correctly.
        let mut filter = HistoryFilter::new();
        let input = b"Hello\x1b[?1004hWorld"; // focus tracking in middle
        let output = filter.filter(input);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("Hello"), "slow path should keep safe text");
        assert!(text.contains("World"), "slow path should keep safe text");
        assert!(
            !text.contains("1004"),
            "slow path should strip unsafe sequences"
        );
    }

    #[test]
    fn test_fast_path_returns_borrowed() {
        let mut filter = HistoryFilter::new();
        let input = b"plain text with \x1b[31mcolors\x1b[0m";
        let output = filter.filter(input.as_slice());
        assert!(
            matches!(output, Cow::Borrowed(_)),
            "fast path should return Cow::Borrowed, got Cow::Owned"
        );
    }

    #[test]
    fn test_slow_path_returns_owned() {
        let mut filter = HistoryFilter::new();
        let input = b"text\x1b[?1004hmore"; // contains blacklisted focus tracking
        let output = filter.filter(input.as_slice());
        assert!(
            matches!(output, Cow::Owned(_)),
            "slow path should return Cow::Owned, got Cow::Borrowed"
        );
    }

    #[test]
    fn test_empty_input() {
        let mut filter = HistoryFilter::new();
        let output = filter.filter(b"");
        assert!(output.is_empty(), "empty input should produce empty output");
        assert!(
            matches!(output, Cow::Borrowed(_)),
            "empty input should return Cow::Borrowed"
        );
    }
}
