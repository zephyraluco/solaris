//! Run with `cargo run -p solaris --release --example transcript_bench --locked`.
//! Synthetic layout timings, not terminal/network latency or a cross-platform claim.
use ratatui::layout::Rect;
use solaris::tui::state::{SessionState, Turn};
use solaris::tui::view::transcript::TranscriptView;
use solaris_tui::components::welcome::WelcomeData;
use solaris_tui::theme::Theme;
use std::hint::black_box;
use std::time::Instant;

fn measure(mut action: impl FnMut()) -> (f64, f64) {
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        action();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    (samples[10], samples[19])
}
fn main() {
    let area = Rect::new(0, 0, 100, 30);
    let theme = Theme::dark();
    let welcome = WelcomeData::default();
    println!(
        "turns,history_bytes,full_p50_ms,full_p95_ms,append_p50_ms,append_p95_ms,animation_p50_ms,animation_p95_ms"
    );
    for count in [10, 100, 300] {
        let body = "## Findings\n\n- **Unicode** 中文 é 👩‍💻 and useful facts to wrap correctly.\n\n```rust\nlet value = 1234567890;\n```\n".repeat(8);
        let mut session = SessionState::new();
        for _ in 0..count {
            session.turns.push(Turn {
                prompt: "Inspect the project".into(),
                reply: body.clone(),
                complete: true,
                ..Default::default()
            });
        }
        session.turns.push(Turn {
            reply: "streaming output".into(),
            ..Default::default()
        });
        let mut view = TranscriptView::new();
        let full = measure(|| {
            view.invalidate();
            black_box(view.lines(&session, area, &theme, Some(0), &welcome));
        });
        let append = measure(|| {
            session.active_turn_mut().unwrap().push_text("x");
            session.bump();
            black_box(view.lines(&session, area, &theme, Some(0), &welcome));
        });
        let mut frame = 0;
        let animation = measure(|| {
            frame += 1;
            black_box(view.lines(&session, area, &theme, Some(frame), &welcome));
        });
        println!(
            "{count},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            body.len() * count,
            full.0,
            full.1,
            append.0,
            append.1,
            animation.0,
            animation.1
        );
    }
}
