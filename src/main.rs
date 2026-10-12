#![cfg_attr(not(windows), allow(dead_code))]
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
macro_rules! logln {
    ($($a:tt)*) => { $crate::log::write(&format!($($a)*)) };
}
mod app;
mod config;
mod codecs;
mod extaudio;
mod fonts;
mod lang;
mod log;
mod mkv;
mod mkvaudio;
mod net;
mod backend;
mod player;
mod report;
mod srcio;
mod subrender;
mod subs;
mod swvideo;
mod vcodecs;

use std::time::{Duration, Instant};

fn headless(cli: config::Cli, secs: f64) {
    // self-test: decode + audio clock + subtitles without a window
    let Some(target) = cli.target.clone() else { eprintln!("need a file"); std::process::exit(2) };
    let mut o = cli.opts.clone();
    if o.hwdec.is_empty() { o.hwdec = "auto".into(); }
    let p = player::Player::start(target, o, 100.0, true);
    let t = Instant::now();
    let (mut frames, mut last_print) = (0u32, Instant::now());
    let mut seeked = false;
    while t.elapsed().as_secs_f64() < secs {
        if let Some(f) = p.poll_frame() {
            frames += 1;
            if frames == 1 { println!("first frame {}x{} pts={:.3}", f.w, f.h, f.pts); }
        }
        if let Some(e) = p.error() { println!("ERROR: {e}"); break; }
        if !seeked && t.elapsed().as_secs_f64() > secs / 2.0 { p.seek_abs(p.position() + 2.0); seeked = true; println!("seek requested"); }
        if last_print.elapsed() > Duration::from_secs(1) {
            let i = p.info();
            println!("t={:.2}s pos={:.2} frames={} hw={} {}x{} dur={:?}", t.elapsed().as_secs_f64(), p.position(), frames, i.hw, i.width, i.height, i.duration);
            last_print = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    let st = p.shared.subs.lock().unwrap();
    for tr in &st.tracks { println!("sub track {} '{}' cues={}", tr.id, tr.label, tr.cues.len()); }
    println!("done: {frames} frames, ended={}", p.ended());
}

fn main() {
    log::install_panic_hook();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = config::parse(argv);
    if cli.help {
        println!("{}", config::HELP);
        return;
    }
    if cli.serve_only {
        let Some(t) = cli.target.clone() else { eprintln!("need a url"); std::process::exit(2) };
        let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        match net::serve(&t, &cli.opts, quit, true) {
            Ok(u) => {
                println!("{u}");
                loop { std::thread::sleep(Duration::from_secs(60)); }
            }
            Err(e) => { eprintln!("serve failed: {e:#}"); std::process::exit(1); }
        }
    }
    if let Some(s) = cli.headless_secs {
        return headless(cli, s);
    }
    let settings = config::Settings::load();
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100.0, 650.0]).with_min_inner_size([480.0, 270.0]).with_title("Rmp").with_drag_and_drop(true),
        vsync: true,
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("Rmp", opts, Box::new(move |cc| Box::new(app::App::new(cc, cli, settings)))) {
        logln!("gui error: {e}");
        std::process::exit(1);
    }
}
