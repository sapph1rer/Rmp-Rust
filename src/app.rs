//! egui front-end: video surface, subtitle overlay, mpv-like auto-hiding controls.

use crate::config::{Cli, Settings};
use crate::player::{Options, Player};
use crate::subrender;
use crate::subs;
use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct ReportUi {
    open: bool,
    text: String,
    media: bool,
    log: bool,
    link: bool,
    settings: bool,
    status: Arc<std::sync::Mutex<String>>,
    sending: Arc<std::sync::atomic::AtomicBool>,
    last_sent: Option<Instant>,
}

impl Default for ReportUi {
    fn default() -> Self {
        ReportUi { open: false, text: String::new(), media: true, log: true, link: false, settings: true, status: Default::default(), sending: Default::default(), last_sent: None }
    }
}

/// Microsoft Store page of a video extension ("HEVC", "VP9", "AV1").
fn open_store(codec: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let id = match codec {
            "HEVC" => "9NMZLZ57R3T7",
            "VP9" => "9N4D0MSMP0PT",
            _ => "9MVZQVXJBQ9V", // AV1
        };
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", &format!("ms-windows-store://pdp/?productid={id}")]).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(not(windows))]
    let _ = codec;
}

pub struct App {
    player: Option<Player>,
    opts: Options,
    settings: Settings,
    tex: Option<TextureHandle>,
    vid_size: Vec2,
    last_move: Instant,
    show_settings: bool,
    url_dialog: Option<String>,
    scrub: Option<f64>,
    fullscreen: bool,
    osd: Option<(String, Instant)>,
    status: String,
    start_paused: bool,
    last_cursor: Pos2,
    current_url: Option<String>,
    exit_on_end: bool,
    exit_on_error: bool,
    titled: bool,
    audio_note_until: Option<Instant>,
    last_note: String,
    settings_tab: u8,
    /// open information window: 0 = media, 1 = codecs
    info_window: Option<u8>,
    report: ReportUi,
    /// subtitle renderer (shaped text) for the font chosen in the settings: key = (font setting, bold)
    sub_r: Option<(String, bool, Option<Arc<subrender::Renderer>>)>,
    sub_cache: HashMap<String, (TextureHandle, Vec2)>,
    /// other qualities of this video (label, url) and the one playing now
    alt_urls: Vec<(String, String)>,
    quality_idx: Option<usize>,
    /// close report for waku
    position_file: Option<String>,
    ended_seen: bool,
    last_pos: f64,
    last_dur: f64,
    last_report: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Play,
    Pause,
    Back,
    Forward,
    Volume,
    Muted,
    Full,
    ExitFull,
    Gear,
}

/// Flat, scalable icon button drawn with vector shapes (the default egui fonts have no icon glyphs).
fn icon_button(ui: &mut egui::Ui, icon: Icon, label: Option<String>, tip: &str) -> egui::Response {
    use egui::{Shape, Stroke};
    let h = ui.spacing().interact_size.y;
    let w = if label.is_some() { h * 1.85 } else { h * 1.35 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    let hovered = resp.hovered();
    let bg = if resp.is_pointer_button_down_on() {
        Color32::from_white_alpha(60)
    } else if hovered {
        Color32::from_white_alpha(38)
    } else {
        Color32::from_white_alpha(14)
    };
    let col = if hovered { Color32::WHITE } else { Color32::from_gray(215) };
    let p = ui.painter();
    p.rect_filled(rect, 5.0, bg);
    let s = h * 0.27;
    let c = rect.center();
    let poly = |pts: Vec<Pos2>| Shape::convex_polygon(pts, col, Stroke::NONE);
    let st = Stroke::new((h * 0.065).max(1.5), col);
    match icon {
        Icon::Play => {
            p.add(poly(vec![Pos2::new(c.x - s * 0.55, c.y - s), Pos2::new(c.x - s * 0.55, c.y + s), Pos2::new(c.x + s * 0.95, c.y)]));
        }
        Icon::Pause => {
            for dx in [-0.7f32, 0.15] {
                p.rect_filled(Rect::from_min_max(Pos2::new(c.x + s * dx, c.y - s), Pos2::new(c.x + s * (dx + 0.55), c.y + s)), 1.0, col);
            }
        }
        Icon::Back | Icon::Forward => {
            let fwd = icon == Icon::Forward;
            let step = s * 0.85;
            let cx = if fwd { rect.left() + h * 1.28 } else { rect.left() + h * 0.52 };
            for k in 0..2 {
                let k = k as f32;
                let (base, apex) = if fwd { (cx - step + step * k, cx + step * k) } else { (cx + step * k, cx - step + step * k) };
                p.add(poly(vec![Pos2::new(base, c.y - s * 0.8), Pos2::new(base, c.y + s * 0.8), Pos2::new(apex, c.y)]));
            }
            if let Some(t) = &label {
                let x = if fwd { rect.left() + h * 0.12 } else { rect.left() + h * 1.08 };
                p.text(Pos2::new(x, c.y), egui::Align2::LEFT_CENTER, t, FontId::proportional(h * 0.45), col);
            }
        }
        Icon::Volume | Icon::Muted => {
            p.rect_filled(Rect::from_min_max(Pos2::new(c.x - s * 1.1, c.y - s * 0.4), Pos2::new(c.x - s * 0.45, c.y + s * 0.4)), 0.0, col);
            p.add(poly(vec![
                Pos2::new(c.x - s * 0.45, c.y - s * 0.4),
                Pos2::new(c.x + s * 0.3, c.y - s * 1.0),
                Pos2::new(c.x + s * 0.3, c.y + s * 1.0),
                Pos2::new(c.x - s * 0.45, c.y + s * 0.4),
            ]));
            if icon == Icon::Volume {
                for r in [0.75f32, 1.3] {
                    let pts: Vec<Pos2> = (0..=8)
                        .map(|i| {
                            let a = (-50.0 + 100.0 * i as f32 / 8.0).to_radians();
                            Pos2::new(c.x + s * 0.3 + s * r * a.cos(), c.y + s * r * a.sin())
                        })
                        .collect();
                    p.add(Shape::line(pts, st));
                }
            } else {
                p.line_segment([Pos2::new(c.x + s * 0.65, c.y - s * 0.55), Pos2::new(c.x + s * 1.45, c.y + s * 0.55)], st);
                p.line_segment([Pos2::new(c.x + s * 0.65, c.y + s * 0.55), Pos2::new(c.x + s * 1.45, c.y - s * 0.55)], st);
            }
        }
        Icon::Full | Icon::ExitFull => {
            let (d, l) = if icon == Icon::Full { (1.0f32, 0.6f32) } else { (0.35, 0.6) };
            for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let pt = Pos2::new(c.x + sx * s * d, c.y + sy * s * d);
                // arms point towards the centre when entering, away from it when exiting
                let dir = if icon == Icon::Full { -1.0 } else { 1.0 };
                p.line_segment([pt, Pos2::new(pt.x + sx * dir * s * l, pt.y)], st);
                p.line_segment([pt, Pos2::new(pt.x, pt.y + sy * dir * s * l)], st);
            }
        }
        Icon::Gear => {
            p.circle_stroke(c, s * 0.55, Stroke::new(st.width * 1.2, col));
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (sn, cs) = a.sin_cos();
                p.line_segment([Pos2::new(c.x + cs * s * 0.85, c.y + sn * s * 0.85), Pos2::new(c.x + cs * s * 1.15, c.y + sn * s * 1.15)], Stroke::new(st.width * 1.8, col));
            }
            p.circle_stroke(c, s * 0.85, Stroke::new(st.width, col));
        }
    }
    resp.on_hover_text(tip)
}

/// Subtitle track picker shared by the control bar and the context menu. Returns true when "import" was clicked.
fn quality_label(alts: &[(String, String)], cur: Option<usize>) -> String {
    cur.and_then(|i| alts.get(i)).map(|x| x.0.clone()).unwrap_or_else(|| "Quality".into())
}

/// Quality list as menu entries; returns the picked index.
fn quality_menu(ui: &mut egui::Ui, alts: &[(String, String)], cur: Option<usize>) -> Option<usize> {
    let mut pick = None;
    for (i, (l, _)) in alts.iter().enumerate() {
        if ui.radio(cur == Some(i), l).clicked() {
            pick = Some(i);
            ui.close_menu();
        }
    }
    pick
}

fn subtitle_menu(ui: &mut egui::Ui, p: &Player, delay_setting: &mut f64) -> bool {
    let mut import = false;
    ui.style_mut().wrap = Some(false);
    ui.set_min_width(220.0);
    let mut st = p.shared.subs.lock().unwrap();
    if ui.selectable_label(st.active.is_none(), "Off").clicked() {
        st.active = None;
        ui.close_menu();
    }
    let mut items: Vec<(usize, String, u8)> = st.tracks.iter().map(|t| (t.id, t.label.clone(), t.pri)).collect();
    items.sort_by_key(|i| i.2); // stable: external files first, then Thai, English, others in file order
    // a long list (30 languages) scrolls inside the menu instead of running off the screen
    egui::ScrollArea::vertical().id_source("sub_menu_scroll").max_height(menu_list_height(ui)).auto_shrink([true, true]).show(ui, |ui| {
        for (id, label, _) in items {
            let on = st.active == Some(id);
            let text = if on { format!("\u{2022} {label}") } else { label };
            if ui.selectable_label(on, text).clicked() {
                st.active = Some(id);
                ui.close_menu();
            }
        }
    });
    if st.loading {
        ui.weak("Loading embedded subtitles...");
    } else if st.tracks.is_empty() {
        ui.weak("No subtitle tracks found");
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Delay");
        if ui.add(egui::DragValue::new(&mut st.delay).speed(0.05).suffix(" s")).changed() {
            *delay_setting = st.delay;
        }
    });
    drop(st);
    if ui.button("Import subtitle file...").clicked() {
        import = true;
        ui.close_menu();
    }
    import
}

/// Height a menu list may use: the window minus room for the rest of the menu (delay, import ...).
fn menu_list_height(ui: &egui::Ui) -> f32 {
    (ui.ctx().screen_rect().height() - 190.0).clamp(120.0, 900.0)
}

fn audio_menu(ui: &mut egui::Ui, p: &Player, info: &crate::player::MediaInfo) {
    ui.style_mut().wrap = Some(false);
    egui::ScrollArea::vertical().id_source("audio_menu_scroll").max_height(menu_list_height(ui)).auto_shrink([true, true]).show(ui, |ui| {
    for t in &info.audio {
        let on = info.cur_audio == Some(t.id);
        let text = if !t.playable {
            format!("{}  [unsupported]", t.label)
        } else if on {
            format!("\u{2022} {}", t.label)
        } else {
            t.label.clone()
        };
        if ui.add_enabled(t.playable, egui::SelectableLabel::new(on, text)).clicked() {
            p.set_audio_track(t.id);
            ui.close_menu();
        }
    }
    });
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0) as u64;
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

fn install_fonts(ctx: &egui::Context) {
    // egui's built-in fonts have no Thai; borrow the best Thai-capable font of the OS for the UI.
    let mut fonts = egui::FontDefinitions::default();
    if let Some((_, f)) = crate::fonts::resolve("", false) {
        fonts.font_data.insert("thai".into(), egui::FontData::from_owned((*f.bytes).clone()));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("thai".into());
        }
    }
    ctx.set_fonts(fonts);
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cli: Cli, mut settings: Settings) -> App {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.style_mut(|s| {
            s.spacing.button_padding = Vec2::new(10.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.item_spacing = Vec2::new(10.0, 8.0);
            s.spacing.slider_rail_height = 7.0;
            for (_, f) in s.text_styles.iter_mut() {
                f.size = (f.size * 1.2).max(15.0);
            }
        });
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::BLACK;
        cc.egui_ctx.set_visuals(visuals);
        if let Some(v) = cli.volume {
            settings.volume = v;
        }
        if cli.mute {
            settings.muted = true;
        }
        let mut opts = cli.opts.clone();
        if opts.hwdec.is_empty() {
            opts.hwdec = settings.hwdec.clone();
        }
        let mut app = App {
            player: None, opts, settings, tex: None, vid_size: Vec2::new(16.0, 9.0), last_move: Instant::now(),
            show_settings: false, url_dialog: None, scrub: None, fullscreen: cli.fullscreen, osd: None,
            status: "Drop a file here, or press Ctrl+O / Ctrl+L".into(), start_paused: cli.pause, last_cursor: Pos2::ZERO,
            current_url: None,
            exit_on_end: cli.target.is_some() && !cli.keep_open,
            exit_on_error: cli.target.is_some(),
            titled: false,
            audio_note_until: None,
            last_note: String::new(),
            settings_tab: 0,
            info_window: None,
            report: ReportUi::default(),
            sub_r: None,
            sub_cache: HashMap::new(),
            quality_idx: cli.target.as_ref().and_then(|t| cli.alt_urls.iter().position(|(_, u)| u == t)),
            alt_urls: cli.alt_urls.clone(),
            position_file: cli.position_file.clone(),
            ended_seen: false,
            last_pos: 0.0,
            last_dur: 0.0,
            last_report: Instant::now(),
        };
        app.apply_live_settings();
        if cli.fullscreen {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        if let Some(t) = cli.target {
            app.open(&cc.egui_ctx, t);
        }
        app
    }

    /// Switch to another rendition at the same position (the link is opened fresh).
    fn switch_quality(&mut self, ctx: &egui::Context, i: usize) {
        let Some((label, url)) = self.alt_urls.get(i).cloned() else { return };
        if self.quality_idx == Some(i) {
            return;
        }
        if let Some(p) = &self.player {
            self.opts.start = p.position();
            if p.is_paused() {
                self.start_paused = true;
            }
        }
        self.quality_idx = Some(i);
        self.open(ctx, url);
        self.osd(format!("Quality: {label}"));
    }

    /// Text for `--position-file`; None when nothing has been played.
    fn report_text(&self, ended: bool) -> Option<String> {
        self.player.as_ref()?;
        let q = self.quality_idx.and_then(|i| self.alt_urls.get(i)).map(|x| x.0.as_str()).unwrap_or("");
        Some(format!("pos={:.1}\nduration={:.1}\nended={}\nquality={}\n", self.last_pos, self.last_dur, u8::from(ended), q))
    }

    fn write_report(&self, ended: bool) {
        if let (Some(f), Some(t)) = (&self.position_file, self.report_text(ended)) {
            let _ = std::fs::write(f, t);
        }
    }

    fn open(&mut self, ctx: &egui::Context, target: String) {
        self.player = None; // drop = stop old threads
        self.current_url = Some(target.clone());
        self.titled = false;
        self.last_note.clear();
        self.tex = None;
        let mut o = self.opts.clone();
        o.hwdec = self.settings.hwdec.clone();
        o.max_height = self.settings.max_height;
        o.audio_langs = crate::lang::parse_list(&self.settings.audio_langs);
        o.sub_langs = crate::lang::parse_list(&self.settings.sub_langs);
        o.audio_delay = self.settings.audio_delay;
        let p = Player::start(target.clone(), o, self.settings.volume, self.settings.muted);
        p.shared.subs.lock().unwrap().delay = self.settings.sub_delay;
        crate::codecs::set_mix(self.settings.mix_center, self.settings.mix_surround, self.settings.mix_lfe);
        if self.start_paused {
            p.set_paused(true);
            self.start_paused = false;
        } else {
            p.set_paused(false); // always auto-play a newly opened file
        }
        self.player = Some(p);
        self.status = format!("Opening {target} ...");
        self.opts.start = 0.0;
        self.opts.external_subs.clear();
        ctx.request_repaint();
    }

    /// Settings that can change while a file plays.
    fn apply_live_settings(&mut self) {
        crate::codecs::set_mix(self.settings.mix_center, self.settings.mix_surround, self.settings.mix_lfe);
        if let Some(p) = &self.player {
            p.shared.audio_delay.store(self.settings.audio_delay);
            p.shared.subs.lock().unwrap().delay = self.settings.sub_delay;
        }
    }

    fn osd(&mut self, s: impl Into<String>) {
        self.osd = Some((s.into(), Instant::now()));
    }

    fn open_file_dialog(&mut self, ctx: &egui::Context) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(p) = rfd::FileDialog::new().pick_file() {
            self.open(ctx, p.to_string_lossy().into_owned());
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = ctx;
            self.url_dialog = Some(String::new()); // no native dialog on Linux build: paste a path
        }
    }

    fn import_sub_dialog(&mut self) {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(p) = rfd::FileDialog::new().add_filter("Subtitles", &["srt", "ass", "ssa", "vtt"]).pick_file() {
            self.add_sub(&p.to_string_lossy());
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        self.osd("Drop a .srt/.ass/.vtt file onto the window to import it");
    }

    fn add_sub(&mut self, path: &str) {
        if let Some(p) = &self.player {
            match p.add_subtitle_file(path) {
                Ok(()) => self.osd("Subtitle loaded"),
                Err(e) => self.osd(format!("Subtitle error: {e}")),
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // no hotkeys while a text box has the keyboard (report note, URL box, settings fields)
        let typing = self.url_dialog.is_some() || ctx.wants_keyboard_input();
        let step = self.settings.seek_step;
        let (mut toggle, mut seek, mut vol, mut mute, mut fs, mut sub_cycle, mut sub_delay, mut sub_toggle, mut open, mut url) =
            (false, 0.0, 0.0f32, false, false, false, 0.0, false, false, false);
        ctx.input(|i| {
            if typing {
                return;
            }
            toggle = i.key_pressed(Key::Space) || i.key_pressed(Key::K);
            if i.key_pressed(Key::ArrowRight) { seek += if i.modifiers.shift { step * 6.0 } else { step }; }
            if i.key_pressed(Key::ArrowLeft) { seek -= if i.modifiers.shift { step * 6.0 } else { step }; }
            if i.key_pressed(Key::L) { seek += step; }
            if i.key_pressed(Key::J) { seek -= step; }
            if i.key_pressed(Key::ArrowUp) { vol += self.settings.volume_step; }
            if i.key_pressed(Key::ArrowDown) { vol -= self.settings.volume_step; }
            mute = i.key_pressed(Key::M);
            fs = i.key_pressed(Key::F) || i.key_pressed(Key::F11);
            sub_cycle = i.key_pressed(Key::S);
            sub_toggle = i.key_pressed(Key::V);
            if i.key_pressed(Key::Z) { sub_delay -= 0.1; }
            if i.key_pressed(Key::X) { sub_delay += 0.1; }
            open = i.modifiers.command && i.key_pressed(Key::O);
            url = i.modifiers.command && i.key_pressed(Key::L);
            if i.key_pressed(Key::Escape) && self.fullscreen {
                fs = true;
            }
        });
        if url {
            self.url_dialog = Some(String::new());
        }
        if open {
            self.open_file_dialog(ctx);
        }
        if fs {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        let Some(p) = &self.player else { return };
        if toggle {
            p.toggle_pause();
        }
        if seek != 0.0 {
            p.seek_rel(seek);
            self.osd = Some((format!("{}{:.0}s", if seek > 0.0 { "+" } else { "" }, seek), Instant::now()));
        }
        if vol != 0.0 {
            let v = (p.volume() + vol).clamp(0.0, 130.0);
            p.set_volume(v);
            self.settings.volume = v;
            self.osd = Some((format!("Volume {v:.0}%"), Instant::now()));
        }
        if mute {
            p.set_muted(!p.muted());
            self.settings.muted = p.muted();
        }
        let mut msg = None;
        {
            let mut st = p.shared.subs.lock().unwrap();
            if sub_cycle {
                st.cycle();
                msg = Some(match st.tracks.iter().find(|t| Some(t.id) == st.active) {
                    Some(t) => format!("Subtitles: {}", t.label),
                    None => "Subtitles: off".to_string(),
                });
            }
            if sub_toggle {
                if st.active.is_some() {
                    st.active = None;
                    msg = Some("Subtitles: off".into());
                } else {
                    st.active = st.tracks.first().map(|t| t.id);
                    msg = Some("Subtitles: on".into());
                }
            }
            if sub_delay != 0.0 {
                st.delay += sub_delay;
                self.settings.sub_delay = st.delay;
                msg = Some(format!("Sub delay {:+.1}s", st.delay));
            }
        }
        if let Some(m) = msg {
            self.osd = Some((m, Instant::now()));
        }
    }

    fn sub_renderer(&mut self) -> Option<Arc<subrender::Renderer>> {
        let key = (self.settings.sub_font.clone(), self.settings.sub_bold);
        if self.sub_r.as_ref().map_or(true, |(f, b, _)| (f.clone(), *b) != key) {
            let r = crate::fonts::resolve(&key.0, key.1).and_then(|(name, fd)| {
                logln!("subtitle font: {name}{}", if key.1 { " (bold)" } else { "" });
                subrender::Renderer::new(fd)
            });
            self.sub_cache.clear();
            self.sub_r = Some((key.0, key.1, r.map(Arc::new)));
        }
        self.sub_r.as_ref().and_then(|(_, _, r)| r.clone())
    }

    fn draw_subs(&mut self, ctx: &egui::Context, painter: &egui::Painter, video: Rect, t: f64) {
        let cues: Vec<(String, bool)> = {
            let Some(p) = &self.player else { return };
            let st = p.shared.subs.lock().unwrap();
            st.active_cues(t).iter().map(|c| (c.text.clone(), c.top)).collect()
        };
        if cues.is_empty() {
            return;
        }
        let ppp = ctx.pixels_per_point();
        let vh = video.height() * ppp; // picture height in physical pixels
        let k = vh / 1080.0;
        let s = &self.settings;
        let style = subrender::Style {
            size: (s.sub_scale / 100.0 * vh).max(14.0),
            outline: s.sub_outline * k,
            shadow: s.sub_shadow * k,
            bg_alpha: (s.sub_bg as f32 * 2.55).round().min(255.0) as u8,
            color: s.sub_color,
            max_width: video.width() * ppp * 0.92,
            line_gap: 0.04,
        };
        let renderer = self.sub_renderer();
        let margin = self.settings.sub_margin / 100.0 * video.height();
        let (mut bottom_y, mut top_y) = (video.bottom() - margin, video.top() + margin * 0.6);
        if self.sub_cache.len() > 24 {
            self.sub_cache.clear();
        }
        for (text, top) in cues.iter().rev() {
            let key = format!("{text}|{:.0}|{:.1}|{:.1}|{}|{:?}|{:.0}", style.size, style.outline, style.shadow, style.bg_alpha, style.color, style.max_width);
            let mut entry = self.sub_cache.get(&key).cloned();
            if entry.is_none() {
                if let Some(r) = renderer.as_ref().filter(|r| r.covers(text)) {
                    if let Some(bm) = r.render(text, &style) {
                        let img = egui::ColorImage::from_rgba_premultiplied([bm.w, bm.h], &bm.rgba);
                        let tex = ctx.load_texture("sub", img, TextureOptions::LINEAR);
                        let e = (tex, Vec2::new(bm.w as f32 / ppp, bm.h as f32 / ppp));
                        self.sub_cache.insert(key.clone(), e.clone());
                        entry = Some(e);
                    }
                }
            }
            if let Some((tex, size)) = entry {
                let x = (video.center().x - size.x / 2.0).round();
                let y = if *top {
                    let y = top_y;
                    top_y += size.y;
                    y
                } else {
                    bottom_y -= size.y;
                    bottom_y
                };
                painter.image(tex.id(), Rect::from_min_size(Pos2::new(x, y.round()), size), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                continue;
            }
            // fallback (characters the subtitle font does not have, e.g. CJK): plain egui text
            let px = style.size / ppp;
            let font = FontId::proportional(px);
            let galley = painter.layout(text.clone(), font.clone(), Color32::WHITE, video.width() * 0.9);
            let (w, h) = (galley.size().x, galley.size().y);
            let pos = Pos2::new(video.center().x - w / 2.0, if *top { let y = top_y; top_y += h; y } else { bottom_y -= h; bottom_y });
            let shadow = painter.layout(text.clone(), font, Color32::BLACK, video.width() * 0.9);
            let o = (px / 16.0).max(1.2);
            for (dx, dy) in [(-o, 0.0), (o, 0.0), (0.0, -o), (0.0, o), (-o, -o), (o, o), (-o, o), (o, -o)] {
                painter.galley(pos + Vec2::new(dx, dy), shadow.clone(), Color32::BLACK);
            }
            painter.galley(pos, galley, Color32::WHITE);
        }
    }

    fn controls(&mut self, ctx: &egui::Context, rect: Rect) {
        let Some(p) = &self.player else { return };
        let info = p.info();
        let pos = self.scrub.unwrap_or_else(|| p.position());
        let dur = info.duration.unwrap_or(0.0);
        // local scale for the bar only (no global ppp changes -> no resize feedback/flicker)
        let k = (rect.height() / 620.0).min(rect.width() / 980.0).clamp(1.0, 2.2);
        let bar_w = rect.width();
        let mut seek_to = None;
        let mut action: Option<&'static str> = None;
        let mut q_pick: Option<usize> = None;
        egui::Area::new("controls".into()).anchor(egui::Align2::LEFT_BOTTOM, [0.0, 0.0]).order(egui::Order::Foreground).show(ctx, |ui| {
            {
                let st = ui.style_mut();
                for f in st.text_styles.values_mut() {
                    f.size *= k;
                }
                st.spacing.interact_size *= k;
                st.spacing.item_spacing *= k;
                st.spacing.button_padding *= k;
                st.spacing.icon_width *= k;
                st.spacing.icon_width_inner *= k;
            }
            ui.set_width(bar_w);
            egui::Frame::none().fill(Color32::from_rgba_unmultiplied(0, 0, 0, 190)).inner_margin(egui::Margin::symmetric(12.0, 6.0)).show(ui, |ui| {
                ui.set_width(bar_w - 24.0);
                // progress
                ui.horizontal(|ui| {
                    ui.label(RichText::new(fmt_time(pos)).monospace());
                    let mut v = pos;
                    ui.spacing_mut().slider_width = (ui.available_width() - 80.0 * k).max(50.0);
                    let r = ui.add_enabled(dur > 0.0, egui::Slider::new(&mut v, 0.0..=dur.max(0.1)).show_value(false).trailing_fill(true));
                    if r.dragged() {
                        self.scrub = Some(v);
                    }
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        seek_to = Some(v);
                        self.scrub = None;
                    }
                    ui.label(RichText::new(if dur > 0.0 { fmt_time(dur) } else { "--:--".into() }).monospace());
                });
                ui.horizontal(|ui| {
                    let paused = p.is_paused() || p.ended();
                    if icon_button(ui, if paused { Icon::Play } else { Icon::Pause }, None, if paused { "Play (Space)" } else { "Pause (Space)" }).clicked() { action = Some("toggle"); }
                    let step = format!("{:.0}", self.settings.seek_step);
                    if icon_button(ui, Icon::Back, Some(step.clone()), "Back (Left)").clicked() { action = Some("back"); }
                    if icon_button(ui, Icon::Forward, Some(step), "Forward (Right)").clicked() { action = Some("fwd"); }
                    ui.separator();
                    let muted = p.muted();
                    if icon_button(ui, if muted { Icon::Muted } else { Icon::Volume }, None, if muted { "Unmute (M)" } else { "Mute (M)" }).clicked() { action = Some("mute"); }
                    let mut v = p.volume();
                    ui.spacing_mut().slider_width = 110.0 * k;
                    if ui.add(egui::Slider::new(&mut v, 0.0..=130.0).show_value(false)).changed() {
                        p.set_volume(v);
                        self.settings.volume = v;
                    }
                    ui.label(format!("{v:.0}%"));
                    ui.separator();
                    ui.menu_button("CC", |ui| {
                        if subtitle_menu(ui, p, &mut self.settings.sub_delay) { action = Some("sub"); }
                    });
                    if info.audio.len() > 1 {
                        ui.menu_button("Audio", |ui| audio_menu(ui, p, &info));
                    }
                    if self.alt_urls.len() > 1 {
                        ui.menu_button(quality_label(&self.alt_urls, self.quality_idx), |ui| q_pick = quality_menu(ui, &self.alt_urls, self.quality_idx));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, if self.fullscreen { Icon::ExitFull } else { Icon::Full }, None, "Fullscreen (F)").clicked() { action = Some("fs"); }
                        if icon_button(ui, Icon::Gear, None, "Settings").clicked() { action = Some("settings"); }
                        ui.label(RichText::new(format!("{}x{} {} [{}]", info.width, info.height, info.vcodec, if info.hw.is_empty() { "..." } else { &info.hw })).small().weak());
                    });
                });
            });
        });
        if let Some(t) = seek_to {
            p.seek_abs(t);
        }
        match action {
            Some("toggle") => p.toggle_pause(),
            Some("back") => p.seek_rel(-self.settings.seek_step),
            Some("fwd") => p.seek_rel(self.settings.seek_step),
            Some("mute") => { p.set_muted(!p.muted()); self.settings.muted = p.muted(); }
            Some("fs") => { self.fullscreen = !self.fullscreen; ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen)); }
            Some("settings") => self.show_settings = !self.show_settings,
            Some("sub") => self.import_sub_dialog(),
            _ => {}
        }
        if let Some(i) = q_pick {
            self.switch_quality(ctx, i);
        }
    }

    fn menu_contents(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.set_min_width(220.0);
        let mut act: Option<&'static str> = None;
        let mut q_pick: Option<usize> = None;
        if let Some(p) = &self.player {
            let paused = p.is_paused();
            if ui.button(if paused { "Play" } else { "Pause" }).clicked() { act = Some("toggle"); }
            if ui.button(format!("Back {:.0}s", self.settings.seek_step)).clicked() { act = Some("back"); }
            if ui.button(format!("Forward {:.0}s", self.settings.seek_step)).clicked() { act = Some("fwd"); }
            ui.separator();
            ui.menu_button("Subtitles", |ui| {
                if subtitle_menu(ui, p, &mut self.settings.sub_delay) {
                    act = Some("sub");
                }
            });
            let info = p.info();
            if info.audio.len() > 1 {
                ui.menu_button("Audio track", |ui| audio_menu(ui, p, &info));
            }
            if self.alt_urls.len() > 1 {
                ui.menu_button(format!("Quality ({})", quality_label(&self.alt_urls, self.quality_idx)), |ui| q_pick = quality_menu(ui, &self.alt_urls, self.quality_idx));
            }
            ui.separator();
        }
        if ui.button("Open file...").clicked() { act = Some("open"); ui.close_menu(); }
        if ui.button("Open URL...").clicked() { act = Some("url"); ui.close_menu(); }
        if ui.button(if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" }).clicked() { act = Some("fs"); ui.close_menu(); }
        if ui.button("Settings...").clicked() { act = Some("settings"); ui.close_menu(); }
        ui.separator();
        if ui.button("Media information").clicked() { act = Some("media"); ui.close_menu(); }
        if ui.button("Codec information").clicked() { act = Some("codec"); ui.close_menu(); }
        if ui.button("Report problem...").clicked() { act = Some("report"); ui.close_menu(); }
        ui.separator();
        if ui.button("Quit").clicked() { act = Some("quit"); }
        match act {
            Some("toggle") => if let Some(p) = &self.player { p.toggle_pause() },
            Some("back") => if let Some(p) = &self.player { p.seek_rel(-self.settings.seek_step) },
            Some("fwd") => if let Some(p) = &self.player { p.seek_rel(self.settings.seek_step) },
            Some("sub") => self.import_sub_dialog(),
            Some("open") => self.open_file_dialog(ctx),
            Some("url") => self.url_dialog = Some(String::new()),
            Some("fs") => { self.fullscreen = !self.fullscreen; ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen)); }
            Some("settings") => self.show_settings = true,
            Some("media") => self.info_window = Some(0),
            Some("codec") => self.info_window = Some(1),
            Some("report") => self.report.open = true,
            Some("quit") => { self.settings.save(); ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
            _ => {}
        }
        if let Some(i) = q_pick {
            self.switch_quality(ctx, i);
        }
    }

    /// Live numbers shown at the bottom of "Media information" and attached to reports.
    fn playback_text(&self) -> String {
        let Some(p) = &self.player else { return "(nothing is playing)\n".into() };
        let i = p.info();
        let mut t = String::new();
        t.push_str("SUBTITLES\n");
        {
            let st = p.shared.subs.lock().unwrap();
            if st.tracks.is_empty() {
                t.push_str(if st.loading { "  (still reading embedded subtitles...)\n" } else { "  (none)\n" });
            }
            for tr in &st.tracks {
                t.push_str(&format!("  {} {}  ({} lines)\n", if Some(tr.id) == st.active { ">" } else { " " }, tr.label, tr.cues.len()));
            }
        }
        t.push_str("\nPLAYBACK\n");
        t.push_str(&format!("  Position:        {}\n", fmt_time(p.position())));
        t.push_str(&format!("  Decoded size:    {}x{} (picture limit: {})\n", i.width, i.height, if self.settings.max_height == 0 { "native".to_string() } else { format!("{}p", self.settings.max_height) }));
        t.push_str(&format!("  Video decoding:  {}\n", i.hw));
        t.push_str(&format!("  Convert time:    {:.1} ms/frame\n", p.shared.conv_us.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0));
        t.push_str(&format!("  Dropped frames:  {}\n", p.shared.dropped.load(std::sync::atomic::Ordering::Relaxed)));
        t.push_str(&format!("  Audio underruns: {}\n", p.shared.underruns.load(std::sync::atomic::Ordering::Relaxed)));
        if let Some(e) = p.error() {
            t.push_str(&format!("  Error:           {e}\n"));
        }
        if !i.audio_note.is_empty() {
            t.push_str(&format!("  Audio note:      {}\n", i.audio_note));
        }
        t
    }

    fn info_windows(&mut self, ctx: &egui::Context) {
        let Some(which) = self.info_window else { return };
        let (title, mut text, missing) = {
            let i = self.player.as_ref().map(|p| p.info()).unwrap_or_default();
            if which == 0 {
                let mut t = if i.media_text.is_empty() { "(nothing loaded)\n".to_string() } else { i.media_text.clone() };
                t.push('\n');
                t.push_str(&self.playback_text());
                ("Media information", t, Vec::new())
            } else {
                ("Codec information", if i.codec_text.is_empty() { "(nothing loaded)\n".to_string() } else { i.codec_text.clone() }, i.missing_codecs.clone())
            }
        };
        let mut open = true;
        egui::Window::new(title).open(&mut open).default_width(640.0).default_height(460.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Copy").clicked() {
                    ctx.copy_text(text.clone());
                }
                for c in &missing {
                    if ui.button(format!("Get {c} extension (Microsoft Store)")).clicked() {
                        open_store(c);
                    }
                }
            });
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut text).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY).interactive(true));
            });
        });
        if !open {
            self.info_window = None;
        }
        ctx.request_repaint_after(Duration::from_millis(500)); // live numbers
    }

    fn report_attachment(&self) -> String {
        let r = &self.report;
        let mut t = format!("{}\nTime: {}\n", crate::report::system_summary(), crate::report::iso_now());
        if let Some(u) = &self.current_url {
            if r.link {
                t.push_str(&format!("Link: {}\n", u.split(['?', '#']).next().unwrap_or(u)));
            } else {
                t.push_str(&format!("Source: {}\n", if u.contains("://") { "link (hidden)" } else { "local file" }));
            }
        }
        if r.media {
            if let Some(p) = &self.player {
                let i = p.info();
                t.push_str("\n===== MEDIA =====\n");
                t.push_str(&i.media_text);
                t.push_str("\n===== CODECS =====\n");
                t.push_str(&i.codec_text);
            }
            t.push('\n');
            t.push_str(&self.playback_text());
        }
        if r.settings {
            t.push_str("\n===== SETTINGS =====\n");
            t.push_str(&serde_json::to_string_pretty(&self.settings).unwrap_or_default());
            t.push('\n');
        }
        if r.log {
            t.push_str("\n===== LOG (end) =====\n");
            t.push_str(&crate::log::tail(200_000));
        }
        t
    }

    fn report_window(&mut self, ctx: &egui::Context) {
        if !self.report.open {
            return;
        }
        let mut open = true;
        let mut send = false;
        egui::Window::new("Report problem").open(&mut open).default_width(520.0).show(ctx, |ui| {
            ui.label("What went wrong? (what you did, what you expected)");
            ui.add(egui::TextEdit::multiline(&mut self.report.text).desired_rows(6).desired_width(f32::INFINITY));
            ui.add_space(6.0);
            ui.label("Attach to the report:");
            ui.checkbox(&mut self.report.media, "Media and codec information");
            ui.checkbox(&mut self.report.log, "Log file (errors, audio/video diagnostics)");
            ui.checkbox(&mut self.report.settings, "My settings");
            ui.checkbox(&mut self.report.link, "The link / address of the video (query string removed)");
            ui.small("Reports go to the developer's Discord channel. Nothing else is sent.");
            ui.add_space(6.0);
            let sending = self.report.sending.load(std::sync::atomic::Ordering::Relaxed);
            let wait = self.report.last_sent.map_or(0, |t| 60u64.saturating_sub(t.elapsed().as_secs()));
            ui.horizontal(|ui| {
                let can = !sending && wait == 0 && !self.report.text.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new(if sending { "Sending..." } else { "Send report" })).clicked() {
                    send = true;
                }
                if wait > 0 {
                    ui.label(format!("wait {wait}s"));
                }
                let st = self.report.status.lock().unwrap().clone();
                if !st.is_empty() {
                    ui.label(st);
                }
            });
        });
        if send {
            let (msg, sys, att) = (self.report.text.clone(), crate::report::system_summary(), self.report_attachment());
            let (status, flag) = (self.report.status.clone(), self.report.sending.clone());
            *status.lock().unwrap() = "Sending...".into();
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            self.report.last_sent = Some(Instant::now());
            std::thread::spawn(move || {
                let r = crate::report::send(&crate::report::webhook_url(), &msg, &sys, &att);
                *status.lock().unwrap() = match r {
                    Ok(()) => "Thank you! Report sent.".into(),
                    Err(e) => e,
                };
                flag.store(false, std::sync::atomic::Ordering::Relaxed);
            });
        }
        if self.report.sending.load(std::sync::atomic::Ordering::Relaxed) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if !open {
            self.report.open = false;
        }
    }

    /// When a file fails because Windows lacks a video decoder: offer the Store page right away.
    fn missing_codec_help(&mut self, ctx: &egui::Context) {
        let Some(e) = self.player.as_ref().and_then(|p| p.error()) else { return };
        let up = e.to_uppercase();
        let which: Vec<&str> = ["HEVC", "VP9", "AV1"].into_iter().filter(|c| up.contains(c)).collect();
        if which.is_empty() || !up.contains("NO DECODER") {
            return;
        }
        egui::Window::new("Video codec missing").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -80.0]).show(ctx, |ui| {
            ui.label("Windows needs a free video extension to play this file:");
            for c in which {
                if ui.button(format!("Get {c} extension (Microsoft Store)")).clicked() {
                    open_store(c);
                }
            }
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut reload = false;
        let before = self.settings.clone();
        // fits the screen: the page scrolls when the window is shorter than its content
        let max_h = (ctx.screen_rect().height() - 90.0).clamp(240.0, 700.0);
        egui::Window::new("Settings").open(&mut open).resizable(true).default_width(500.0).max_height(max_h).constrain(true).show(ctx, |ui| {
            ui.spacing_mut().slider_width = 170.0;
            ui.horizontal(|ui| {
                for (i, name) in ["General", "Video", "Audio", "Subtitles"].iter().enumerate() {
                    ui.selectable_value(&mut self.settings_tab, i as u8, *name);
                }
            });
            ui.separator();
            let st = &mut self.settings;
            egui::ScrollArea::vertical().id_source("settings_scroll").auto_shrink([false, true]).max_height((max_h - 120.0).max(120.0)).show(ui, |ui| {
            egui::Grid::new("set").num_columns(2).spacing([16.0, 8.0]).min_col_width(170.0).show(ui, |ui| match self.settings_tab {
                0 => {
                    ui.label("Skip step (s)");
                    ui.add(egui::DragValue::new(&mut st.seek_step).clamp_range(1.0..=300.0));
                    ui.end_row();
                    ui.label("Volume step (%)");
                    ui.add(egui::DragValue::new(&mut st.volume_step).clamp_range(1.0..=25.0));
                    ui.end_row();
                    ui.label("Hide controls after (s)");
                    ui.add(egui::DragValue::new(&mut st.autohide_secs).clamp_range(0.5..=10.0));
                    ui.end_row();
                }
                1 => {
                    ui.label("GPU decoding (D3D11/DXVA)");
                    egui::ComboBox::from_id_source("hwdec").selected_text(if st.hwdec == "no" { "Off (software)" } else { "Auto (GPU)" }).show_ui(ui, |ui| {
                        if ui.selectable_value(&mut st.hwdec, "auto".to_string(), "Auto (GPU)").clicked() { reload = true; }
                        if ui.selectable_value(&mut st.hwdec, "no".to_string(), "Off (software)").clicked() { reload = true; }
                    });
                    ui.end_row();
                    ui.label("Picture quality limit");
                    let name = |h: u32| match h { 0 => "Native - no downscale (best)".to_string(), 720 => "720p (lightest)".into(), 1080 => "1080p".into(), 1440 => "1440p".into(), h => format!("{h}p") };
                    egui::ComboBox::from_id_source("maxh").selected_text(name(st.max_height)).show_ui(ui, |ui| {
                        for h in [720u32, 1080, 1440, 0] {
                            if ui.selectable_value(&mut st.max_height, h, name(h)).clicked() { reload = true; }
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    ui.small("Native shows every pixel. Pick a limit only if a 4K file stutters on a weak PC.\nHEVC / AV1 / VP9 need the matching Microsoft Store video extension (see Codec information).");
                    ui.end_row();
                }
                2 => {
                    ui.strong("Track and timing");
                    ui.label("");
                    ui.end_row();
                    ui.label("Preferred audio languages");
                    ui.add(egui::TextEdit::singleline(&mut st.audio_langs).hint_text("e.g. Thai, Japanese").desired_width(200.0));
                    ui.end_row();
                    ui.label("Audio delay (s)");
                    ui.add(egui::DragValue::new(&mut st.audio_delay).speed(0.01).clamp_range(-10.0..=10.0).suffix(" s"));
                    ui.end_row();
                    ui.strong("Stereo mix (surround to 2 speakers)");
                    ui.label("");
                    ui.end_row();
                    ui.label("Dialogue level (stereo mix)");
                    ui.add(egui::Slider::new(&mut st.mix_center, 0.5..=2.0).fixed_decimals(2));
                    ui.end_row();
                    ui.label("Surround level");
                    ui.add(egui::Slider::new(&mut st.mix_surround, 0.0..=1.5).fixed_decimals(2));
                    ui.end_row();
                    ui.label("Bass (LFE) level");
                    ui.add(egui::Slider::new(&mut st.mix_lfe, 0.0..=1.0).fixed_decimals(2));
                    ui.end_row();
                    ui.label("");
                    ui.small("Languages apply when a file is opened. Mix levels apply to DTS, TrueHD, Opus and Vorbis.");
                    ui.end_row();
                }
                _ => {
                    ui.strong("Language");
                    ui.label("");
                    ui.end_row();
                    ui.label("Preferred subtitle languages");
                    ui.add(egui::TextEdit::singleline(&mut st.sub_langs).hint_text("e.g. Thai, English").desired_width(200.0));
                    ui.end_row();
                    ui.strong("Text");
                    ui.label("");
                    ui.end_row();
                    ui.label("Font");
                    let shown = if st.sub_font.is_empty() { "Automatic".to_string() } else { st.sub_font.clone() };
                    egui::ComboBox::from_id_source("subfont").selected_text(shown).width(200.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut st.sub_font, String::new(), "Automatic");
                        for n in crate::fonts::available() {
                            ui.selectable_value(&mut st.sub_font, n.to_string(), n);
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut st.sub_bold, "Bold");
                    ui.end_row();
                    ui.label("Size (% of picture height)");
                    ui.add(egui::Slider::new(&mut st.sub_scale, 2.0..=10.0).fixed_decimals(1));
                    ui.end_row();
                    ui.label("Colour");
                    ui.color_edit_button_srgb(&mut st.sub_color);
                    ui.end_row();
                    ui.strong("Outline and background");
                    ui.label("");
                    ui.end_row();
                    ui.label("Outline");
                    ui.add(egui::Slider::new(&mut st.sub_outline, 0.0..=6.0).fixed_decimals(1));
                    ui.end_row();
                    ui.label("Shadow");
                    ui.add(egui::Slider::new(&mut st.sub_shadow, 0.0..=6.0).fixed_decimals(1));
                    ui.end_row();
                    ui.label("Background box (%)");
                    ui.add(egui::Slider::new(&mut st.sub_bg, 0..=100));
                    ui.end_row();
                    ui.strong("Position and timing");
                    ui.label("");
                    ui.end_row();
                    ui.label("Distance from bottom (%)");
                    ui.add(egui::Slider::new(&mut st.sub_margin, 0.0..=25.0).fixed_decimals(1));
                    ui.end_row();
                    ui.label("Delay (s)");
                    ui.add(egui::DragValue::new(&mut st.sub_delay).speed(0.05).suffix(" s"));
                    ui.end_row();
                }
            });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Reset all to defaults").clicked() {
                    let keep = (self.settings.volume, self.settings.muted);
                    self.settings = Settings::default();
                    self.settings.volume = keep.0;
                    self.settings.muted = keep.1;
                    reload = before.hwdec != self.settings.hwdec || before.max_height != self.settings.max_height;
                }
                if reload {
                    ui.label("Reloading...");
                } else {
                    ui.small("Saved automatically.");
                }
            });
        });
        self.show_settings = open;
        self.apply_live_settings();
        if reload {
            // restart at the current position with the new decoder
            if let Some(p) = &self.player {
                self.opts.start = p.position();
            }
            if let Some(url) = self.current_url.clone() {
                self.open(ctx, url);
            }
        }
        self.settings.save();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // drag & drop: subtitle files are imported, anything else is opened
        let dropped: Vec<String> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned())).collect());
        for path in dropped {
            if subs::is_sub_ext(&path) && self.player.is_some() {
                self.add_sub(&path);
            } else {
                self.open(ctx, path);
            }
        }
        self.handle_keys(ctx);

        if let Some(p) = &self.player {
            if self.exit_on_error {
                if let Some(e) = p.error() {
                    logln!("error: {e}");
                    std::process::exit(2); // like mpv: "file could not be loaded"
                }
            }
            {
                // a new note (e.g. "No sound: cannot read this audio track") is shown for a few seconds
                let n = p.info().audio_note;
                if n != self.last_note {
                    if !n.is_empty() {
                        self.audio_note_until = Some(Instant::now() + Duration::from_secs(8));
                    }
                    self.last_note = n;
                }
            }
            if !self.titled {
                let i = p.info();
                if i.loaded {
                    self.titled = true;
                    if !i.audio_note.is_empty() {
                        self.audio_note_until = Some(Instant::now() + Duration::from_secs(8));
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} - Rmp", i.title)));
                }
            }
            if p.ended() {
                self.ended_seen = true;
            } else if self.last_report.elapsed() >= Duration::from_secs(3) {
                // keep the close report fresh (also survives a crash / kill)
                self.last_report = Instant::now();
                self.last_pos = p.position();
                self.last_dur = p.info().duration.unwrap_or(self.last_dur);
                self.write_report(false);
            }
            if self.exit_on_end && p.ended() {
                self.settings.save();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // pull the newest due video frame into the GPU texture
        if let Some(p) = &self.player {
            if let Some(f) = p.poll_frame() {
                let img = egui::ColorImage { size: [f.w as usize, f.h as usize], pixels: f.pixels };
                self.vid_size = Vec2::new(f.w as f32, f.h as f32);
                match &mut self.tex {
                    Some(t) => t.set(img, TextureOptions::LINEAR),
                    None => self.tex = Some(ctx.load_texture("video", img, TextureOptions::LINEAR)),
                }
            }
            if let Some(e) = p.error() {
                self.status = format!("Error: {e}");
            } else if p.info().loaded && self.tex.is_none() && !p.info().has_video {
                self.status = "Audio only".into();
            }
        }

        let mut rect = Rect::NOTHING;
        let mut clicked = false;
        let mut dbl = false;
        egui::CentralPanel::default().frame(egui::Frame::none().fill(Color32::BLACK)).show(ctx, |ui| {
            rect = ui.max_rect();
            let resp = ui.allocate_rect(rect, Sense::click());
            clicked = resp.clicked();
            dbl = resp.double_clicked();
            let resp_menu = resp.clone();
            if let Some(t) = &self.tex {
                let scale = (rect.width() / self.vid_size.x).min(rect.height() / self.vid_size.y);
                let sz = self.vid_size * scale;
                let r = Rect::from_center_size(rect.center(), sz);
                ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                let pos = self.player.as_ref().map(|p| p.position());
                if let Some(pos) = pos {
                    self.draw_subs(ctx, ui.painter(), r, pos);
                }
            } else {
                let msg = self.player.as_ref().and_then(|p| p.error()).map(|e| format!("Error: {e}")).unwrap_or_else(|| self.status.clone());
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, msg, FontId::proportional(18.0), Color32::GRAY);
            }
            resp_menu.context_menu(|ui| self.menu_contents(ui, ctx));
            if let Some((m, t0)) = &self.osd {
                if t0.elapsed() < Duration::from_millis(1500) {
                    ui.painter().text(rect.left_top() + Vec2::new(16.0, 44.0), egui::Align2::LEFT_TOP, m, FontId::proportional(22.0), Color32::WHITE);
                }
            }
            if let (Some(p), Some(until)) = (&self.player, self.audio_note_until) {
                let n = p.info().audio_note;
                if Instant::now() < until && !n.is_empty() {
                    ui.painter().text(rect.left_top() + Vec2::new(16.0, 80.0), egui::Align2::LEFT_TOP, n, FontId::proportional(18.0), Color32::from_rgb(255, 200, 80));
                }
            }
            if let Some(p) = &self.player {
                let i = p.info();
                if !i.title.is_empty() && self.last_move.elapsed().as_secs_f32() < self.settings.autohide_secs {
                    ui.painter().text(rect.left_top() + Vec2::new(16.0, 14.0), egui::Align2::LEFT_TOP, &i.title, FontId::proportional(15.0), Color32::from_gray(200));
                }
            }
        });
        if clicked && !self.show_settings {
            if let Some(p) = &self.player {
                p.toggle_pause();
            }
        }
        if dbl {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        // auto-hide the control bar like mpv
        let cur = ctx.input(|i| i.pointer.hover_pos()).unwrap_or(self.last_cursor);
        if cur != self.last_cursor || ctx.input(|i| i.pointer.any_down()) {
            self.last_move = Instant::now();
            self.last_cursor = cur;
        }
        let paused = self.player.as_ref().map_or(true, |p| p.is_paused());
        let show = self.player.is_some() && (paused || self.show_settings || self.last_move.elapsed().as_secs_f32() < self.settings.autohide_secs);
        if show {
            self.controls(ctx, rect);
        } else if self.fullscreen {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        if self.show_settings {
            self.settings_window(ctx);
        }
        self.info_windows(ctx);
        self.report_window(ctx);
        self.missing_codec_help(ctx);

        // "open URL / path" box (Ctrl+L)
        if let Some(buf) = &mut self.url_dialog {
            let mut go = None;
            let mut close = false;
            egui::Window::new("Open URL or file path").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(460.0).hint_text("https://.../video.m3u8  or  /path/to/file.mkv"));
                r.request_focus();
                ui.horizontal(|ui| {
                    if ui.button("Play").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
                        go = Some(buf.trim().trim_matches('"').to_string());
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
            if let Some(u) = go.filter(|u| !u.is_empty()) {
                self.url_dialog = None;
                self.open(ctx, u);
            } else if close {
                self.url_dialog = None;
            }
        }

        // keep repainting while something is playing
        if self.player.as_ref().map_or(false, |p| !p.is_paused()) || self.osd.is_some() {
            ctx.request_repaint_after(Duration::from_millis(8));
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(p) = &self.player {
            if !p.ended() {
                self.last_pos = p.position();
            }
            self.last_dur = p.info().duration.unwrap_or(self.last_dur);
            let ended = self.ended_seen || p.ended();
            self.write_report(ended);
        }
        self.settings.save();
    }
}
