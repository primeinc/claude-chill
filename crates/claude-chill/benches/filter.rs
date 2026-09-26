use claude_chill::history_filter::HistoryFilter;
use claude_chill::sync_block::SyncBlockParser;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

/// Generate realistic terminal output: mix of text, SGR colors, and cursor movement.
fn generate_terminal_output(size_kb: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size_kb * 1024);
    let mut line = 0;
    while data.len() < size_kb * 1024 {
        // Colored text line (common case)
        let content = format!(
            "\x1b[38;2;{};{};{}m  {:>6} | fn process_output(&mut self, data: &[u8]) -> Result<()> {{\x1b[0m\r\n",
            (line * 7) % 256,
            (line * 13) % 256,
            (line * 31) % 256,
            line + 1
        );
        data.extend_from_slice(content.as_bytes());
        line += 1;
    }
    data
}

/// Generate output with mode-setting sequences that must be filtered out.
fn generate_mixed_output(size_kb: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size_kb * 1024);
    let mut line = 0;
    while data.len() < size_kb * 1024 {
        // Regular text
        let text = format!("\x1b[32m  Line {line}\x1b[0m\r\n");
        data.extend_from_slice(text.as_bytes());

        // Every 10th line: inject a mode-setting sequence that should be filtered
        if line % 10 == 0 {
            // Focus tracking mode (should be blacklisted)
            data.extend_from_slice(b"\x1b[?1004h");
            // Mouse mode (should be blacklisted)
            data.extend_from_slice(b"\x1b[?1000h");
            // Bracketed paste (should be blacklisted)
            data.extend_from_slice(b"\x1b[?2004h");
        }
        line += 1;
    }
    data
}

fn bench_filter_clean(c: &mut Criterion) {
    let mut group = c.benchmark_group("history_filter_clean");

    for size_kb in [10, 100, 1000] {
        let data = generate_terminal_output(size_kb);
        group.throughput(criterion::Throughput::Bytes(data.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size_kb}KB")),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut filter = HistoryFilter::new();
                    filter.filter(black_box(data))
                });
            },
        );
    }
    group.finish();
}

fn bench_filter_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("history_filter_mixed");

    for size_kb in [10, 100, 1000] {
        let data = generate_mixed_output(size_kb);
        group.throughput(criterion::Throughput::Bytes(data.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size_kb}KB")),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut filter = HistoryFilter::new();
                    filter.filter(black_box(data))
                });
            },
        );
    }
    group.finish();
}

fn bench_sync_parser(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_block_parser");

    for size_kb in [10, 100, 1000] {
        // Wrap output in sync blocks (typical Claude Code pattern)
        let inner = generate_terminal_output(size_kb);
        let mut data = Vec::with_capacity(inner.len() + 100);
        // Multiple sync blocks
        for chunk in inner.chunks(4096) {
            data.extend_from_slice(b"\x1b[?2026h");
            data.extend_from_slice(chunk);
            data.extend_from_slice(b"\x1b[?2026l");
        }

        group.throughput(criterion::Throughput::Bytes(data.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size_kb}KB")),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut parser = SyncBlockParser::new();
                    let mut segments = Vec::new();
                    parser.parse(black_box(data), &mut segments);
                    segments.len()
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_filter_clean,
    bench_filter_mixed,
    bench_sync_parser
);
criterion_main!(benches);
