//! Plays a file or URL without a window and prints what the pipeline does — a quick way to
//! check a format on a machine without looking at the picture.
//!
//! `cargo run --release --example headless -- <file-or-url> [seconds] [seek-to]`

use std::time::{Duration, Instant};

use playr::player::{Player, State};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(target) = args.next() else {
        eprintln!("usage: headless <file-or-url> [seconds] [seek-to]");
        std::process::exit(2);
    };
    let seconds: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let seek_to: Option<f64> = args.next().and_then(|s| s.parse().ok());

    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.05);
    player.open(target);

    let started = Instant::now();
    let (mut frames, mut last_pts, mut worst_late) = (0u64, f64::NAN, 0.0f64);
    let mut printed_open = false;
    let mut sought = false;
    let mut next_report = 1.0;
    loop {
        let status = player.shared.status();
        if !printed_open && !matches!(status.state, State::Loading | State::Idle) {
            printed_open = true;
            println!(
                "opened in {:.0} ms: state={:?} title={:?} info={:?} duration={:?} seekable={} video={:?} audio={}",
                started.elapsed().as_secs_f64() * 1e3,
                status.state, status.title, status.info, status.duration, status.seekable, status.video, status.has_audio
            );
            if let Some(e) = &status.error {
                println!("error: {e}");
            }
        }
        let (frame, _) = player.shared.take_due_frame();
        if let Some(f) = frame {
            if frames == 0 {
                let (cx, cy) = (f.width / 2, f.height / 2);
                let y = f.y[(cy * f.y_stride + cx) as usize];
                let u = f.u[(cy / 2 * f.c_stride + cx / 2) as usize];
                let v = f.v[(cy / 2 * f.c_stride + cx / 2) as usize];
                println!(
                    "first frame: pts={:.3} {}x{} chroma {}x{} strides {}/{} centre YUV=({y},{u},{v})",
                    f.pts, f.width, f.height, f.chroma_width, f.chroma_height, f.y_stride, f.c_stride
                );
            }
            frames += 1;
            last_pts = f.pts;
            worst_late = worst_late.max(player.shared.position() - f.pts);
        }
        let elapsed = started.elapsed().as_secs_f64();
        if let (Some(to), false) = (seek_to, sought)
            && elapsed > seconds / 2.0 {
                sought = true;
                println!("seek -> {to}");
                player.seek(to);
                worst_late = 0.0;
            }
        if elapsed >= next_report {
            next_report += 1.0;
            println!(
                "t={elapsed:4.1}s pos={:7.3} frames={frames} last_pts={last_pts:.3} worst_late={:.0}ms buffered={:.2}s now={:?} notes={:?}",
                player.shared.position(),
                worst_late * 1e3,
                player.shared.audio.buffered_secs(),
                status.now_playing,
                status.notes
            );
        }
        if elapsed >= seconds || matches!(status.state, State::Ended | State::Failed) && printed_open {
            println!("final: state={:?} pos={:.3} frames={frames} error={:?}", status.state, player.shared.position(), status.error);
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
