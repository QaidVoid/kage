//! Benchmarks for history rendering at scale.
//!
//! [`render`] benchmarks a representative short conversation. These
//! benchmarks answer a different question: what does a resumed
//! long-running session cost? A thousand-turn session is ~4k blocks;
//! the first paint after `kage resume` must measure every block
//! height (markdown parse plus `syntect` highlight per assistant
//! block), and every steady-state frame walks all of them even
//! though only a viewport is visible.
//!
//! - `scale/cold`: fresh buffer, first paint - the resume freeze.
//! - `scale/clone`: a full buffer snapshot; the per-frame deep copy
//!   the app now skips by parking and reusing the drawn snapshot.
//! - `scale/warm`: primed caches, steady-state per-frame cost.
//! - `scale/scroll`: warm caches with a moving viewport.
//! - `scale/key`: one typed character, one backspace and a scroll key
//!   up and down, dispatched through the App over the scaled buffer
//!   with the embedded default keymap.
//!
//! Run with `cargo bench -p kage-tui --bench scale`. The buffers are
//! large; a reduced sample keeps wall time sane:
//! `-- --sample-size 10 --warm-up-time 1 --measurement-time 3`.

use std::collections::BTreeMap;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use kage_tui::view::{self, CapturedCell, StatusCtx};
use kage_tui::{App, Buffer, Heights, InputState, input_height_for, shared_buffer, split};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Viewport width used for every render benchmark.
const TERM_WIDTH: u16 = 120;
/// Viewport height used for every render benchmark.
const TERM_HEIGHT: u16 = 40;
/// Conversation turns in the scaled buffer. Each turn adds a user
/// prompt, a markdown assistant answer with a fenced code block, and
/// an expanded tool call/result pair (4 blocks per turn).
const TURNS: usize = 1000;

/// Markdown answer for one turn, including a fenced code block so
/// the render exercises the `syntect` highlight path.
fn assistant_markdown(turn: usize) -> String {
    format!(
        "Turn {turn}: after a thousand of these the resume-time first\n\
         paint has measured every block in the session.\n\n\
         ```rust\n\
         fn render(buffer: &mut Buffer) {{\n    \
         for block in buffer.blocks() {{\n        \
         paint(block);\n    }}\n}}\n\
         ```\n\n\
         A resize invalidates the whole cache and pays it again."
    )
}

/// Multi-line tool output for one turn.
fn tool_output(turn: usize) -> String {
    (0..40)
        .map(|line| format!("turn {turn} line {line}: PASS some::module::case_{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the large buffer with every block expanded.
fn scale_buffer() -> Buffer {
    let mut buffer = Buffer::new();
    for turn in 0..TURNS {
        buffer.push_user(format!(
            "Question {turn}: what does per-frame cost look like once\n\
             the session has thousands of blocks?"
        ));
        buffer.begin_assistant();
        buffer.append_assistant_delta(&assistant_markdown(turn));
        let call_id = format!("call-{turn}");
        buffer.push_tool_call(
            call_id.clone(),
            "bash",
            serde_json::json!({ "command": "cargo nextest run" }),
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
            let regions = split(
                frame.area(),
                Heights {
                    header: 1,
                    input: input_height_for(1),
                    ..Heights::default()
                },
            );
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

fn bench_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("scale");

    group.bench_function("cold", |b| {
        let mut terminal = bench_terminal();
        b.iter_batched_ref(
            scale_buffer,
            |buffer| draw(&mut terminal, buffer),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("clone", |b| {
        let buffer = scale_buffer();
        b.iter(|| std::hint::black_box(buffer.clone()));
    });

    group.bench_function("warm", |b| {
        let mut terminal = bench_terminal();
        let mut buffer = scale_buffer();
        draw(&mut terminal, &mut buffer);
        b.iter(|| draw(&mut terminal, &mut buffer));
    });

    group.bench_function("scroll", |b| {
        let mut terminal = bench_terminal();
        let mut buffer = scale_buffer();
        draw(&mut terminal, &mut buffer);
        let span = buffer.total_lines().max(1);
        let mut at = 0usize;
        b.iter(|| {
            at = (at + 97) % span;
            buffer.set_scroll(at);
            draw(&mut terminal, &mut buffer);
        });
    });

    group.bench_function("key", |b| {
        let buffer = shared_buffer();
        *buffer.lock().expect("buffer") = scale_buffer();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(buffer, tx);
        let runtime = kage_plugin::PluginRuntime::new().expect("plugin runtime");
        kage_plugin::load_all(None, &runtime).expect("default keymap");
        app.set_keymap(runtime.keymap());
        let keys = [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL),
        ];
        b.iter(|| {
            for key in keys {
                std::hint::black_box(app.handle_key(key));
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_scale);
criterion_main!(benches);
