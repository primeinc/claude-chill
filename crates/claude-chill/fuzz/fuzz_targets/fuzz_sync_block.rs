#![no_main]

use claude_chill::sync_block::SyncBlockParser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes through the sync block parser.
    // Should never panic regardless of input.
    let mut parser = SyncBlockParser::new();
    let mut segments = Vec::new();
    parser.parse(data, &mut segments);

    // Also test split input: feed data in two halves
    if data.len() >= 2 {
        let mid = data.len() / 2;
        let mut parser2 = SyncBlockParser::new();
        let mut segments2 = Vec::new();
        parser2.parse(&data[..mid], &mut segments2);
        parser2.parse(&data[mid..], &mut segments2);
    }
});
