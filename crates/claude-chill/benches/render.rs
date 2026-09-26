use claude_chill::vt_renderer::VtRenderer;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

/// Generate a realistic sync block: cursor home + colored text lines.
fn generate_sync_block(lines: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(lines * 100);
    // Cursor home
    data.extend_from_slice(b"\x1b[H");
    for i in 0..lines {
        // SGR color + text + newline (typical Claude Code output)
        let line = format!(
            "\x1b[38;5;{}m  Line {:>5}: The quick brown fox jumps over the lazy dog\x1b[0m\r\n",
            (i % 256),
            i + 1
        );
        data.extend_from_slice(line.as_bytes());
    }
    data
}

fn bench_full_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("vt_render_full");

    for lines in [100, 500, 1000, 5000] {
        let data = generate_sync_block(lines);
        group.bench_with_input(BenchmarkId::from_parameter(lines), &data, |b, data| {
            b.iter(|| {
                let mut renderer = VtRenderer::new(50, 120);
                renderer.process(black_box(data));
                renderer.mark_pending();
                let output = renderer.render();
                black_box(output.map(|b| b.len()))
            });
        });
    }
    group.finish();
}

fn bench_diff_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("vt_render_diff");

    for lines in [100, 500, 1000, 5000] {
        let initial = generate_sync_block(lines);
        // Change ~10% of lines for the diff
        let mut updated = initial.clone();
        let changed_line =
            "\x1b[38;5;196m  CHANGED: This line was modified for the benchmark\x1b[0m\r\n"
                .to_string();
        // Replace some bytes near the middle
        let mid = updated.len() / 2;
        let end = (mid + changed_line.len()).min(updated.len());
        updated[mid..end].copy_from_slice(&changed_line.as_bytes()[..end - mid]);

        group.bench_with_input(
            BenchmarkId::from_parameter(lines),
            &(initial, updated),
            |b, (initial, updated)| {
                b.iter(|| {
                    let mut renderer = VtRenderer::new(50, 120);
                    // First render (full)
                    renderer.process(initial);
                    renderer.mark_pending();
                    let _ = renderer.render();
                    // Second render (diff)
                    renderer.process(black_box(updated));
                    renderer.mark_pending();
                    let output = renderer.render();
                    black_box(output.map(|b| b.len()))
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_full_render, bench_diff_render);
criterion_main!(benches);
