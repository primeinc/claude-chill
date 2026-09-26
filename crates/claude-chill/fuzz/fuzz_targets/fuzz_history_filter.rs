#![no_main]

use claude_chill::history_filter::HistoryFilter;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes through the history filter.
    // Should never panic regardless of input.
    // The filter parses escape sequences via termwiz and classifies them.
    let mut filter = HistoryFilter::new();
    let _ = filter.filter(data);

    // Also test incremental feeding: split into small chunks
    let mut filter2 = HistoryFilter::new();
    for chunk in data.chunks(17) {
        let _ = filter2.filter(chunk);
    }
});
