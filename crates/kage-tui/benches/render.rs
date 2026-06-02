//! Benchmarks guarding the TUI render path against regression.
//!
//! [`kage_tui::view::render`] is the hot loop: every frame it walks the
//! conversation buffer, builds wrapped and syntax-highlighted
//! [`ratatui::text::Line`]s for the visible blocks, paints bubbles, and
//! overlays any selection. Block heights and rendered lines are cached
//! per width so steady-state frames clone cached lines instead of
//! rebuilding, and off-screen blocks are skipped entirely.
//!
//! These benchmarks render a representative conversation (user prompts,
//! markdown answers with fenced code, and expanded tool call/result
//! pairs with multi-line output) through a `TestBackend` and measure:
//!
//! - `render/cold`: a fresh buffer each iteration, so the height and
//!   line caches are empty. This is the full build pass (line wrapping
//!   plus `syntect` highlighting) the renderer pays on first paint.
//! - `render/warm`: the same buffer redrawn with primed caches, the
//!   steady-state per-frame cost the idle loop pays at its refresh
//!   target.
//! - `render/scroll`: warm caches but a moving viewport, exercising the
//!   virtualized "which blocks are visible" path without rebuilding
//!   every line.
//!
//! Run with `cargo bench -p kage-tui`. Save a baseline with
//! `cargo bench -p kage-tui -- --save-baseline main` and compare a
//! later run against it with `--baseline main`.

use std::collections::BTreeMap;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use kage_tui::view::{self, CapturedCell, StatusCtx};
use kage_tui::{Buffer, InputState, input_height_for, split};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Viewport width used for every render benchmark.
const TERM_WIDTH: u16 = 120;
/// Viewport height used for every render benchmark.
const TERM_HEIGHT: u16 = 40;
/// Conversation turns in the representative buffer. Each turn adds a
/// user prompt, an assistant answer, and a tool call/result pair.
const TURNS: usize = 60;

/// Markdown answer for one turn, including a fenced code block so the
/// render exercises the `syntect` highlight path.
fn assistant_markdown(turn: usize) -> String {
    format!(
        "Turn {turn}: the renderer caches block heights and lines per width,\n\
         so an unchanged frame clones cached lines rather than rebuilding them.\n\n\
         ```rust\n\
         fn render(buffer: &mut Buffer) {{\n    \
         for block in buffer.blocks() {{\n        \
         paint(block);\n    }}\n}}\n\
         ```\n\n\
         A resize changes the width key and invalidates the whole cache."
    )
}

/// Multi-line tool output for one turn, long enough that an expanded
/// result spans many rows.
fn tool_output(turn: usize) -> String {
    (0..40)
        .map(|line| format!("turn {turn} line {line}: PASS some::module::case_{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a representative conversation buffer with every block expanded,
/// so the cold render measures the full line-building cost rather than
/// folded one-line headers.
fn representative_buffer() -> Buffer {
    let mut buffer = Buffer::new();
    for turn in 0..TURNS {
        buffer.push_user(format!(
            "Question {turn}: walk me through the render cache and the\n\
             virtualization that keeps scrolling cheap."
        ));
        buffer.begin_assistant();
        buffer.append_assistant_delta(&assistant_markdown(turn));
        let call_id = format!("call-{turn}");
        buffer.push_tool_call(
            call_id.clone(),
            "bash",
            "bash(\"cargo nextest run\")",
            "{\n  \"command\": \"cargo nextest run\"\n}",
        );
        buffer.push_tool_result_with_duration(call_id, tool_output(turn), false, Some(420));
    }
    buffer.finish_streaming();
    buffer.set_all_folded(false);
    buffer
}

/// Paint `buffer` once into `terminal` through the full `view::render`
/// entry point with default input, status, and overlay state.
fn draw(terminal: &mut Terminal<TestBackend>, buffer: &mut Buffer) {
    let input = InputState::new();
    let status = StatusCtx::default();
    let mut captured: BTreeMap<usize, Vec<CapturedCell>> = BTreeMap::new();
    terminal
        .draw(|frame| {
            let regions = split(frame.area(), input_height_for(1), 0);
            view::render(
                frame,
                regions,
                buffer,
                &input,
                None,
                &status,
                None,
                &mut captured,
                None,
                &[],
            );
        })
        .expect("render into test backend");
}

/// New `TestBackend` terminal sized for the benchmarks.
fn bench_terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(TERM_WIDTH, TERM_HEIGHT)).expect("test backend")
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");

    group.bench_function("cold", |b| {
        let mut terminal = bench_terminal();
        b.iter_batched_ref(
            representative_buffer,
            |buffer| draw(&mut terminal, buffer),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("warm", |b| {
        let mut terminal = bench_terminal();
        let mut buffer = representative_buffer();
        draw(&mut terminal, &mut buffer);
        b.iter(|| draw(&mut terminal, &mut buffer));
    });

    group.bench_function("scroll", |b| {
        let mut terminal = bench_terminal();
        let mut buffer = representative_buffer();
        draw(&mut terminal, &mut buffer);
        let span = buffer.total_lines().max(1);
        let mut at = 0usize;
        b.iter(|| {
            at = (at + 7) % span;
            buffer.set_scroll(at);
            draw(&mut terminal, &mut buffer);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
