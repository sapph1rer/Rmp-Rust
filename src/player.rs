//! Playback engine: demux thread -> video / audio decoder threads -> shared queues.
//! The UI only talks to `Player`; it pulls RGBA frames with `poll_frame()` and the
//! audio callback (cpal) pulls samples from the same `Shared` state.
//!
//! Clock: audio is the master clock (samples actually played). Without audio
//! (or after the audio track ends) a wall clock is used.

use crate::subs::{self, SubStore};
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FRAME_Q_MAX: usize = 10;

// ------------------------------------------------------------------ public types

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<String>,
    pub demuxer_opts: Vec<(String, String)>,
    pub force_format: Option<String>,
    pub hwdec: String,
    pub start: f64,
    pub title: Option<String>,
    pub external_subs: Vec<String>,
    /// preferred audio / subtitle languages in order ("Thai", "jpn" ...)
    pub audio_langs: Vec<String>,
    pub sub_langs: Vec<String>,
    /// user audio delay in seconds
    pub audio_delay: f64,
    /// cap on the decoded picture height (0 = native)
    pub max_height: u32,
}

pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub pixels: Vec<eframe::egui::Color32>,
    pub pts: f64,
    pub serial: u64,
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub id: usize,
    pub label: String,
    /// false when this machine cannot decode the track
    pub playable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub loaded: bool,
    pub title: String,
    pub duration: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub vcodec: String,
    pub hw: String,
    pub audio: Vec<TrackInfo>,
    pub cur_audio: Option<usize>,
    pub has_video: bool,
    /// why there is no sound (empty when audio works)
    pub audio_note: String,
    /// ready-made text for the "Media information" / "Codec information" windows
    pub media_text: String,
    pub codec_text: String,
    /// video codecs Windows cannot decode on this PC ("HEVC", "AV1", "VP9") -> Store links
    pub missing_codecs: Vec<String>,
}

pub(crate) enum Cmd {
    Seek(f64),
    SetAudio(usize),
}


// ------------------------------------------------------------------- shared state

pub(crate) struct AtomicF64(AtomicU64);
impl AtomicF64 {
    pub(crate) fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    pub(crate) fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Relaxed))
    }
    pub(crate) fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Relaxed)
    }
}

pub struct AudioBuf {
    pub(crate) q: VecDeque<f32>,
    pub(crate) pts_front: f64,
    pub(crate) has_clock: bool,
    pub(crate) rate: u32,
}

pub(crate) struct Clock {
    pub(crate) base_pts: f64,
    pub(crate) base_instant: Instant,
    pub(crate) running: bool,
}
impl Clock {
    fn position(&self) -> f64 {
        if self.running {
            self.base_pts + self.base_instant.elapsed().as_secs_f64()
        } else {
            self.base_pts
        }
    }
}

pub struct Shared {
    pub quit: Arc<AtomicBool>,
    pub paused: AtomicBool,
    pub muted: AtomicBool,
    pub(crate) volume: AtomicU32, // f32 bits, 0..130
    pub(crate) serial: AtomicU64,
    pub(crate) cmd_pending: AtomicUsize,
    pub(crate) seek_target: AtomicF64,
    /// position requested by the UI but not yet applied by the reader thread (NaN = none)
    pub(crate) req_pos: AtomicF64,
    /// diagnostics: audio device underruns, frames dropped for being late, avg picture-convert time (us)
    pub(crate) underruns: AtomicU64,
    pub(crate) dropped: AtomicU64,
    pub(crate) conv_us: AtomicU64,
    pub(crate) t0: AtomicF64,
    /// true once `t0` (first timestamp of the media) is known
    pub(crate) t0_ready: AtomicBool,
    /// user audio delay in seconds (positive = sound later)
    pub(crate) audio_delay: AtomicF64,
    pub(crate) shown_serial: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub info: Mutex<MediaInfo>,
    pub(crate) frames: Mutex<VecDeque<Frame>>,
    pub(crate) frames_cv: Condvar,
    pub(crate) audio: Mutex<AudioBuf>,
    pub(crate) audio_cv: Condvar,
    pub(crate) clock: Mutex<Clock>,
    pub subs: Arc<Mutex<SubStore>>,
    pub(crate) has_audio: AtomicBool,
    pub(crate) has_video: AtomicBool,
    pub(crate) video_eof: AtomicBool,
    pub(crate) audio_eof: AtomicBool,
    pub(crate) out_rate: AtomicU32,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Shared {
            quit: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            volume: AtomicU32::new(100f32.to_bits()),
            serial: AtomicU64::new(1),
            cmd_pending: AtomicUsize::new(0),
            seek_target: AtomicF64::new(0.0),
            req_pos: AtomicF64::new(f64::NAN),
            underruns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            conv_us: AtomicU64::new(0),
            t0: AtomicF64::new(0.0),
            t0_ready: AtomicBool::new(false),
            audio_delay: AtomicF64::new(0.0),
            shown_serial: AtomicU64::new(0),
            error: Mutex::new(None),
            info: Mutex::new(MediaInfo::default()),
            frames: Mutex::new(VecDeque::new()),
            frames_cv: Condvar::new(),
            audio: Mutex::new(AudioBuf { q: VecDeque::new(), pts_front: 0.0, has_clock: false, rate: 48000 }),
            audio_cv: Condvar::new(),
            clock: Mutex::new(Clock { base_pts: 0.0, base_instant: Instant::now(), running: false }),
            subs: Arc::new(Mutex::new(SubStore::default())),
            has_audio: AtomicBool::new(false),
            has_video: AtomicBool::new(false),
            video_eof: AtomicBool::new(false),
            audio_eof: AtomicBool::new(false),
            out_rate: AtomicU32::new(48000),
        }
    }

    /// Short message for the on-screen note (audio / video problems); the UI shows it when it changes.
    pub(crate) fn set_note(&self, msg: &str) {
        let mut i = self.info.lock().unwrap();
        if i.audio_note.is_empty() {
            i.audio_note = msg.to_string();
        } else if !i.audio_note.contains(msg) {
            i.audio_note = format!("{}; {msg}", i.audio_note);
        }
    }

    pub(crate) fn stale(&self, serial: u64) -> bool {
        self.quit.load(Relaxed) || self.serial.load(SeqCst) != serial
    }

    /// Current playback position in seconds (media time, start offset removed).
    pub fn position(&self) -> f64 {
        let r = self.req_pos.load();
        if !r.is_nan() {
            return r;
        }
        if self.has_audio.load(Relaxed) {
            let mut a = self.audio.lock().unwrap();
            if a.has_clock {
                if self.audio_eof.load(Relaxed) && a.q.is_empty() {
                    // audio track finished: carry on with the wall clock
                    let p = a.pts_front;
                    a.has_clock = false;
                    let mut c = self.clock.lock().unwrap();
                    c.base_pts = p;
                    c.base_instant = Instant::now();
                    c.running = !self.paused.load(Relaxed);
                    return p;
                }
                return a.pts_front;
            }
        }
        self.clock.lock().unwrap().position()
    }

    /// Called from the cpal callback: fills `out` with interleaved stereo f32.
    pub(crate) fn audio_pull(&self, out: &mut [f32]) {
        if self.paused.load(Relaxed) {
            out.fill(0.0);
            return;
        }
        let v = f32::from_bits(self.volume.load(Relaxed)) / 100.0;
        // quadratic below 100 %, boosted above; the soft limiter below keeps boosts from clipping
        let gain = if self.muted.load(Relaxed) {
            0.0
        } else if v <= 1.0 {
            v * v
        } else {
            1.0 + (v - 1.0) * 2.0
        };
        let mut a = self.audio.lock().unwrap();
        let n = out.len().min(a.q.len());
        for o in out.iter_mut().take(n) {
            *o = soft_limit(a.q.pop_front().unwrap_or(0.0) * gain);
        }
        out[n..].fill(0.0);
        if n < out.len() && a.has_clock && !self.audio_eof.load(Relaxed) {
            self.underruns.fetch_add(1, Relaxed);
        }
        if a.has_clock {
            let r = a.rate as f64;
            a.pts_front += (n / 2) as f64 / r;
        }
        drop(a);
        if n > 0 {
            self.audio_cv.notify_all();
        }
    }

    pub(crate) fn push_audio(&self, serial: u64, pts: f64, samples: &[f32]) {
        let cap = self.out_rate.load(Relaxed) as usize * 4; // ~2 seconds (stereo samples)
        let mut a = self.audio.lock().unwrap();
        loop {
            if self.stale(serial) {
                return;
            }
            if a.q.len() < cap {
                break;
            }
            a = self.audio_cv.wait_timeout(a, Duration::from_millis(20)).unwrap().0;
        }
        if !a.has_clock || a.q.is_empty() {
            a.pts_front = pts;
            a.has_clock = true;
        }
        a.q.extend(samples.iter().copied());
    }

    pub(crate) fn push_frame(&self, f: Frame) {
        let mut q = self.frames.lock().unwrap();
        loop {
            if self.stale(f.serial) {
                return;
            }
            if q.len() < FRAME_Q_MAX {
                break;
            }
            q = self.frames_cv.wait_timeout(q, Duration::from_millis(15)).unwrap().0;
        }
        q.push_back(f);
    }
}

// ------------------------------------------------------------------- Player API

pub struct Player {
    pub shared: Arc<Shared>,
    tx: Sender<Cmd>,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.quit.store(true, SeqCst);
    }
}

impl Player {
    pub fn start(url: String, opts: Options, volume: f32, muted: bool) -> Player {
        let shared = Arc::new(Shared::new());
        shared.volume.store(volume.to_bits(), Relaxed);
        shared.muted.store(muted, Relaxed);
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        thread::Builder::new()
            .name("demux".into())
            .spawn(move || {
                if let Err(e) = crate::backend::run(sh.clone(), url, opts, rx) {
                    *sh.error.lock().unwrap() = Some(format!("{e:#}"));
                }
            })
            .expect("spawn demux thread");
        Player { shared, tx }
    }

    fn send(&self, c: Cmd) {
        self.shared.cmd_pending.fetch_add(1, SeqCst);
        let _ = self.tx.send(c);
    }

    pub fn position(&self) -> f64 {
        self.shared.position()
    }
    pub fn info(&self) -> MediaInfo {
        self.shared.info.lock().unwrap().clone()
    }
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Relaxed)
    }
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Relaxed))
    }
    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 130.0).to_bits(), Relaxed);
    }
    pub fn muted(&self) -> bool {
        self.shared.muted.load(Relaxed)
    }
    pub fn set_muted(&self, m: bool) {
        self.shared.muted.store(m, Relaxed);
    }

    pub fn set_paused(&self, p: bool) {
        let sh = &self.shared;
        if p == sh.paused.load(Relaxed) {
            return;
        }
        if p {
            let pos = sh.position();
            let mut c = sh.clock.lock().unwrap();
            c.base_pts = pos;
            c.running = false;
            sh.paused.store(true, SeqCst);
        } else {
            sh.paused.store(false, SeqCst); // clock restarts in poll_frame()
        }
    }
    pub fn toggle_pause(&self) {
        // at the end of the file, play = start over
        if self.ended() {
            self.seek_abs(0.0);
            self.set_paused(false);
            return;
        }
        self.set_paused(!self.is_paused());
    }

    pub fn seek_abs(&self, t: f64) {
        let mut t = t.max(0.0);
        if let Some(d) = self.info().duration {
            t = t.min((d - 0.1).max(0.0));
        }
        self.shared.req_pos.store(t);
        self.send(Cmd::Seek(t));
    }
    pub fn seek_rel(&self, dt: f64) {
        self.seek_abs(self.position() + dt);
    }
    pub fn set_audio_track(&self, id: usize) {
        self.send(Cmd::SetAudio(id));
    }

    pub fn add_subtitle_file(&self, path: &str) -> Result<()> {
        add_external_sub(&self.shared, path)
    }

    pub fn ended(&self) -> bool {
        let sh = &self.shared;
        if !sh.info.lock().unwrap().loaded {
            return false;
        }
        let v_done = !sh.has_video.load(Relaxed) || (sh.video_eof.load(SeqCst) && sh.frames.lock().unwrap().is_empty());
        let a_done = !sh.has_audio.load(Relaxed) || (sh.audio_eof.load(SeqCst) && sh.audio.lock().unwrap().q.is_empty());
        v_done && a_done
    }

    /// Returns the newest frame that is due for display (if any).
    pub fn poll_frame(&self) -> Option<Frame> {
        let sh = &self.shared;
        let serial = sh.serial.load(SeqCst);
        let paused = sh.paused.load(Relaxed);
        let mut q = sh.frames.lock().unwrap();
        while q.front().map_or(false, |f| f.serial != serial) {
            q.pop_front();
        }
        let front_pts = q.front()?.pts;

        // first frame after open/seek is shown immediately, even when paused
        if sh.shown_serial.load(Relaxed) != serial {
            sh.shown_serial.store(serial, Relaxed);
            let f = q.pop_front();
            drop(q);
            sh.frames_cv.notify_all();
            return f;
        }
        if paused {
            return None;
        }
        // (re)start the wall clock on the first frame if audio isn't driving it
        {
            let audio_ok = sh.has_audio.load(Relaxed) && sh.audio.lock().unwrap().has_clock;
            if !audio_ok {
                let mut c = sh.clock.lock().unwrap();
                if !c.running {
                    c.base_pts = front_pts;
                    c.base_instant = Instant::now();
                    c.running = true;
                }
            }
        }
        let pos = sh.position();
        let mut out = None;
        let mut popped = 0u64;
        while q.front().map_or(false, |f| f.pts <= pos + 0.002) {
            out = q.pop_front();
            popped += 1;
        }
        if popped > 1 {
            sh.dropped.fetch_add(popped - 1, Relaxed);
        }
        drop(q);
        if out.is_some() {
            sh.frames_cv.notify_all();
        }
        out
    }
}

pub(crate) fn add_external_sub(sh: &Shared, path: &str) -> Result<()> {
    let cues = subs::load_file(path)?;
    let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
    let mut st = sh.subs.lock().unwrap();
    let id = 10_000 + st.next_ext;
    st.next_ext += 1;
    let label = match crate::lang::from_filename(path) {
        Some(l) => format!("{l} (external)"),
        None => name.clone(),
    };
    st.add_track(id, label);
    st.set_priority(id, 0);
    st.set_cues(id, cues);
    st.active = Some(id);
    Ok(())
}


// ------------------------------------------------------------------ audio output
/// Smooth limiter: linear up to 0.8, then compresses towards 1.0 instead of hard clipping.
fn soft_limit(x: f32) -> f32 {
    const K: f32 = 0.8;
    let a = x.abs();
    if a <= K {
        x
    } else {
        x.signum() * (K + (1.0 - K) * ((a - K) / (1.0 - K)).tanh())
    }
}

pub(crate) fn start_audio_output(sh: Arc<Shared>) -> Option<u32> {
    let (tx, rx) = mpsc::channel::<Option<u32>>();
    thread::Builder::new()
        .name("audio-out".into())
        .spawn(move || {
            use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
            let dev = match cpal::default_host().default_output_device() {
                Some(d) => d,
                None => {
                    let _ = tx.send(None);
                    return;
                }
            };
            let cfg = match dev.default_output_config() {
                Ok(c) => c,
                Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            };
            let rate = cfg.sample_rate().0;
            let ch = cfg.channels() as usize;
            let fmt = cfg.sample_format();
            let sc: cpal::StreamConfig = cfg.into();
            sh.out_rate.store(rate, Relaxed);
            let stream = match fmt {
                cpal::SampleFormat::F32 => build_stream::<f32>(&dev, &sc, ch, sh.clone()),
                cpal::SampleFormat::I16 => build_stream::<i16>(&dev, &sc, ch, sh.clone()),
                cpal::SampleFormat::U16 => build_stream::<u16>(&dev, &sc, ch, sh.clone()),
                cpal::SampleFormat::I32 => build_stream::<i32>(&dev, &sc, ch, sh.clone()),
                _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
            };
            match stream {
                Ok(s) if s.play().is_ok() => {
                    let _ = tx.send(Some(rate));
                    while !sh.quit.load(Relaxed) {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                _ => {
                    let _ = tx.send(None);
                }
            }
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
}

fn build_stream<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, ch: usize, sh: Arc<Shared>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let mut tmp: Vec<f32> = Vec::new();
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let frames = data.len() / ch;
            tmp.resize(frames * 2, 0.0);
            sh.audio_pull(&mut tmp);
            for f in 0..frames {
                let (l, r) = (tmp[2 * f], tmp[2 * f + 1]);
                for c in 0..ch {
                    let v = match (ch, c) {
                        (1, _) => (l + r) * 0.5,
                        (_, 0) => l,
                        (_, 1) => r,
                        _ => 0.0,
                    };
                    data[f * ch + c] = T::from_sample(v);
                }
            }
        },
        |e| logln!("audio stream error: {e}"),
        None,
    )
}
