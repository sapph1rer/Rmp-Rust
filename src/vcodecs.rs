//! Software video decoders in pure Rust, used when Windows has no decoder for a codec
//! (HEVC / VP9 / AV1 without the Microsoft Store extensions): oxideav-h265, oxideav-vp9 and rav1d
//! (the Rust port of dav1d, built without its assembly). Output is planar YUV, converted to
//! RGB for the screen by `to_pixels`.

use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, TimeBase};
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VKind {
    Hevc,
    Vp9,
    Av1,
}

impl VKind {
    pub fn from_mkv(codec_id: &str) -> Option<VKind> {
        match codec_id {
            "V_MPEGH/ISO/HEVC" => Some(VKind::Hevc),
            "V_VP9" => Some(VKind::Vp9),
            "V_AV1" => Some(VKind::Av1),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            VKind::Hevc => "HEVC",
            VKind::Vp9 => "VP9",
            VKind::Av1 => "AV1",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Mono,
    C420,
    C422,
    C444,
}

pub struct Plane {
    /// bytes; samples are one byte (8-bit) or little-endian u16 (10/12-bit)
    pub data: Vec<u8>,
    pub stride: usize,
}

pub struct Picture {
    pub pts: f64,
    pub w: usize,
    pub h: usize,
    pub bits: u32,
    pub layout: Layout,
    pub planes: Vec<Plane>,
}

trait Backend: Send {
    fn decode(&mut self, pkt: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), String>;
    fn flush(&mut self, out: &mut Vec<Picture>);
    fn reset(&mut self);
}

pub struct VideoDecoder {
    inner: Box<dyn Backend>,
    pub kind: VKind,
}

impl VideoDecoder {
    /// `private` = Matroska CodecPrivate (hvcC / av1C), `w`/`h` = coded size from the track header.
    pub fn new(kind: VKind, private: &[u8], w: u32, h: u32) -> Result<VideoDecoder, String> {
        let inner: Box<dyn Backend> = match kind {
            VKind::Hevc => Box::new(Oxide::new("h265", private, w, h, hevc_bits(private))?),
            VKind::Vp9 => Box::new(Oxide::new("vp9", &[], w, h, 0)?),
            VKind::Av1 => Box::new(Av1::new(private)?),
        };
        Ok(VideoDecoder { inner, kind })
    }

    /// Feed one container packet; decoded pictures (in display order) are appended to `out`.
    /// A panic inside a decoder only loses that packet.
    pub fn decode(&mut self, pkt: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), String> {
        let inner = &mut self.inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.decode(pkt, pts, out))) {
            Ok(r) => r,
            Err(_) => Err(format!("{} decoder panicked", self.kind.name())),
        }
    }

    pub fn flush(&mut self, out: &mut Vec<Picture>) {
        let inner = &mut self.inner;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.flush(out)));
    }

    pub fn reset(&mut self) {
        let inner = &mut self.inner;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.reset()));
    }
}

/// Luma bit depth from an hvcC record (byte 17 low 3 bits + 8).
fn hevc_bits(hvcc: &[u8]) -> u32 {
    if hvcc.len() > 17 && hvcc[0] == 1 {
        (hvcc[17] & 7) as u32 + 8
    } else {
        8
    }
}

// ------------------------------------------------------------------------------ oxideav (HEVC, VP9)

struct Oxide {
    dec: Box<dyn Decoder>,
    params: (CodecId, Vec<u8>, u32, u32, u32),
    w: usize,
    h: usize,
    bits_hint: u32,
    pts_fallback: VecDeque<f64>,
}

impl Oxide {
    fn make(id: &str, private: &[u8], w: u32, h: u32) -> Result<Box<dyn Decoder>, String> {
        let mut p = CodecParameters::video(CodecId::new(id));
        p.extradata = private.to_vec();
        p.width = Some(w);
        p.height = Some(h);
        let r = if id == "vp9" { oxideav_vp9::make_decoder(&p) } else { oxideav_h265::make_decoder(&p) };
        r.map_err(|e| format!("{e:?}"))
    }

    fn new(id: &str, private: &[u8], w: u32, h: u32, bits: u32) -> Result<Oxide, String> {
        Ok(Oxide { dec: Self::make(id, private, w, h)?, params: (CodecId::new(id), private.to_vec(), w, h, bits), w: w as usize, h: h as usize, bits_hint: bits, pts_fallback: VecDeque::new() })
    }

    fn drain(&mut self, out: &mut Vec<Picture>) {
        while let Ok(f) = self.dec.receive_frame() {
            let Frame::Video(v) = f else { continue };
            let planes = v.image_planes();
            if planes.is_empty() || self.w == 0 {
                continue;
            }
            // sample size from the stride of the luma plane versus the coded width
            let bps = if planes[0].stride >= self.w * 2 { 2 } else { 1 };
            let w = planes[0].stride / bps;
            let h = planes[0].data.len() / planes[0].stride.max(1);
            let layout = if planes.len() < 3 {
                Layout::Mono
            } else {
                let (cw, ch) = (planes[1].stride / bps, planes[1].data.len() / planes[1].stride.max(1));
                if cw * 2 <= w + 1 && ch * 2 <= h + 1 {
                    Layout::C420
                } else if cw * 2 <= w + 1 {
                    Layout::C422
                } else {
                    Layout::C444
                }
            };
            let bits = if bps == 1 {
                8
            } else {
                v.significant_bits().and_then(|b| b.first().copied()).map(|b| b as u32).filter(|b| (9..=16).contains(b)).unwrap_or(if self.bits_hint > 8 { self.bits_hint } else { 10 })
            };
            let pts = v.pts.map(|p| p as f64 / 1e6).or_else(|| self.pts_fallback.pop_front()).unwrap_or(0.0);
            out.push(Picture { pts, w, h, bits, layout, planes: planes.iter().map(|p| Plane { data: p.data.clone(), stride: p.stride }).collect() });
        }
    }
}

impl Backend for Oxide {
    fn decode(&mut self, pkt: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), String> {
        let mut p = Packet::new(0, TimeBase::new(1, 1_000_000), pkt.to_vec());
        p.pts = Some((pts * 1e6).round() as i64);
        self.dec.send_packet(&p).map_err(|e| format!("{e:?}"))?;
        self.drain(out);
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<Picture>) {
        let _ = self.dec.flush();
        self.drain(out);
    }
    fn reset(&mut self) {
        // simplest reliable reset: a fresh decoder (the next packet after a seek is a keyframe)
        if let Ok(d) = Self::make(self.params.0.as_str(), &self.params.1, self.params.2, self.params.3) {
            self.dec = d;
        }
    }
}

// ------------------------------------------------------------------------------ AV1 (rav1d)

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush, dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data};
use std::mem::MaybeUninit;
use std::ptr::NonNull;

struct Av1 {
    ctx: Option<Dav1dContext>,
    config_obus: Vec<u8>,
    pts_q: VecDeque<f64>,
}

// the context is only ever used from the one video thread
unsafe impl Send for Av1 {}

const EAGAIN: i32 = -11;

impl Av1 {
    fn open() -> Result<Dav1dContext, String> {
        unsafe {
            let mut s = MaybeUninit::<Dav1dSettings>::zeroed();
            dav1d_default_settings(NonNull::new(s.as_mut_ptr()).unwrap());
            let mut s = s.assume_init();
            s.n_threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16) as i32;
            s.max_frame_delay = 1; // lowest latency: one frame in flight, tiles/rows run in parallel
            let mut ctx: Option<Dav1dContext> = None;
            let r = dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut s));
            if r.0 != 0 || ctx.is_none() {
                return Err(format!("AV1 decoder start failed ({})", r.0));
            }
            Ok(ctx.unwrap())
        }
    }

    fn new(private: &[u8]) -> Result<Av1, String> {
        // av1C: 4 bytes of config, then optional config OBUs (sequence header)
        let config_obus = if private.len() > 4 { private[4..].to_vec() } else { Vec::new() };
        Ok(Av1 { ctx: Some(Self::open()?), config_obus, pts_q: VecDeque::new() })
    }

    fn send(&mut self, bytes: &[u8], out: &mut Vec<Picture>, pts: f64) -> Result<(), String> {
        let ctx = self.ctx.clone();
        unsafe {
            let mut data = MaybeUninit::<Dav1dData>::zeroed().assume_init();
            let p = dav1d_data_create(NonNull::new(&mut data), bytes.len());
            if p.is_null() {
                return Err("AV1: out of memory".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            self.pts_q.push_back(pts);
            loop {
                let r = dav1d_send_data(ctx.clone(), NonNull::new(&mut data));
                if r.0 != 0 && r.0 != EAGAIN {
                    dav1d_data_unref(NonNull::new(&mut data));
                    self.pts_q.pop_back();
                    return Err(format!("AV1 decode error ({})", r.0));
                }
                self.pull(out);
                if data.sz == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    fn pull(&mut self, out: &mut Vec<Picture>) {
        unsafe {
            loop {
                let mut pic = MaybeUninit::<Dav1dPicture>::zeroed().assume_init();
                let r = dav1d_get_picture(self.ctx.clone(), NonNull::new(&mut pic));
                if r.0 != 0 {
                    break;
                }
                let (w, h, bpc) = (pic.p.w as usize, pic.p.h as usize, pic.p.bpc as u32);
                let layout = match pic.p.layout {
                    0 => Layout::Mono,
                    1 => Layout::C420,
                    2 => Layout::C422,
                    _ => Layout::C444,
                };
                let bps = if bpc > 8 { 2 } else { 1 };
                let mut planes = Vec::new();
                let n = if layout == Layout::Mono { 1 } else { 3 };
                for i in 0..n {
                    let (pw, ph) = if i == 0 {
                        (w, h)
                    } else {
                        match layout {
                            Layout::C420 => ((w + 1) / 2, (h + 1) / 2),
                            Layout::C422 => ((w + 1) / 2, h),
                            _ => (w, h),
                        }
                    };
                    let stride = pic.stride[if i == 0 { 0 } else { 1 }];
                    let base = pic.data[i].map_or(std::ptr::null(), |p| p.as_ptr() as *const u8);
                    let row = pw * bps;
                    let mut data = vec![0u8; row * ph];
                    if !base.is_null() {
                        for y in 0..ph {
                            std::ptr::copy_nonoverlapping(base.offset(y as isize * stride), data.as_mut_ptr().add(y * row), row);
                        }
                    }
                    planes.push(Plane { data, stride: row });
                }
                dav1d_picture_unref(NonNull::new(&mut pic));
                let pts = self.pts_q.pop_front().unwrap_or(0.0);
                out.push(Picture { pts, w, h, bits: bpc, layout, planes });
            }
        }
    }
}

impl Backend for Av1 {
    fn decode(&mut self, pkt: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), String> {
        if !self.config_obus.is_empty() {
            let cfg = std::mem::take(&mut self.config_obus);
            // the sequence header from av1C primes the decoder (no picture comes out of it)
            let ctx = self.ctx.clone();
            unsafe {
                let mut data = MaybeUninit::<Dav1dData>::zeroed().assume_init();
                let p = dav1d_data_create(NonNull::new(&mut data), cfg.len());
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(cfg.as_ptr(), p, cfg.len());
                    let _ = dav1d_send_data(ctx, NonNull::new(&mut data));
                    if data.sz != 0 {
                        dav1d_data_unref(NonNull::new(&mut data));
                    }
                }
            }
        }
        self.send(pkt, out, pts)
    }
    fn flush(&mut self, out: &mut Vec<Picture>) {
        self.pull(out);
    }
    fn reset(&mut self) {
        unsafe { dav1d_flush(self.ctx.clone().unwrap()) };
        self.pts_q.clear();
    }
}

impl Drop for Av1 {
    fn drop(&mut self) {
        unsafe {
            let mut c = self.ctx.take();
            dav1d_close(NonNull::new(&mut c));
        }
    }
}

// ------------------------------------------------------------------------------ YUV -> screen

/// YUV picture -> opaque screen pixels (BT.709 for HD, BT.601 for SD, limited range), several threads.
pub fn to_pixels(p: &Picture) -> Vec<eframe::egui::Color32> {
    let (w, h) = (p.w, p.h);
    let mut out = vec![eframe::egui::Color32::BLACK; w * h];
    if w == 0 || h == 0 || p.planes.is_empty() {
        return out;
    }
    let hd = w >= 1280 || h > 576;
    // fixed-point coefficients (x256)
    let (cr_r, cb_g, cr_g, cb_b) = if hd { (459, 55, 136, 541) } else { (409, 100, 208, 516) };
    let shift = p.bits.saturating_sub(8);
    let wide = p.bits > 8;
    let sample = |pl: &Plane, x: usize, y: usize| -> i32 {
        if wide {
            let i = y * pl.stride + x * 2;
            (u16::from_le_bytes([pl.data[i], pl.data[i + 1]]) >> shift) as i32
        } else {
            pl.data[y * pl.stride + x] as i32
        }
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let rows_per = (h + threads - 1) / threads;
    let (sx, sy) = match p.layout {
        Layout::C420 => (1, 1),
        Layout::C422 => (1, 0),
        _ => (0, 0),
    };
    std::thread::scope(|s| {
        for (bi, band) in out.chunks_mut(rows_per * w).enumerate() {
            let sample = &sample;
            s.spawn(move || {
                for (ry, row) in band.chunks_mut(w).enumerate() {
                    let y = bi * rows_per + ry;
                    for (x, px) in row.iter_mut().enumerate() {
                        let yy = (sample(&p.planes[0], x, y) - 16).max(0) * 298;
                        let (u, v) = if p.layout == Layout::Mono { (128, 128) } else { (sample(&p.planes[1], x >> sx, y >> sy) - 128, sample(&p.planes[2], x >> sx, y >> sy) - 128) };
                        let r = (yy + cr_r * v + 128) >> 8;
                        let g = (yy - cb_g * u - cr_g * v + 128) >> 8;
                        let b = (yy + cb_b * u + 128) >> 8;
                        *px = eframe::egui::Color32::from_rgb(r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8);
                    }
                }
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn run(file: &str, max_frames: usize) -> Option<(usize, f64, Picture)> {
        let path = format!("/tmp/claude-0/v/{file}.mkv");
        if !std::path::Path::new(&path).exists() {
            return None;
        }
        let tracks = crate::mkv::read_tracks(&path)?;
        let t = tracks.iter().find(|t| t.ttype == 1)?.clone();
        let kind = VKind::from_mkv(&t.codec)?;
        let mut dec = VideoDecoder::new(kind, &t.private, t.width, t.height).unwrap();
        let mut dm = crate::mkvaudio::Demux::open(&path, t.number)?;
        let mut pics = Vec::new();
        let start = Instant::now();
        let mut n = 0;
        while let Some((pts, pkt)) = dm.next_packet() {
            if let Err(e) = dec.decode(&pkt, pts, &mut pics) {
                panic!("{file}: {e}");
            }
            n += 1;
            if n >= max_frames {
                break;
            }
        }
        dec.flush(&mut pics);
        let secs = start.elapsed().as_secs_f64();
        let last = pics.pop()?;
        Some((pics.len() + 1, secs, last))
    }

    #[test]
    fn decodes_and_measures() {
        for f in ["hevc", "hevc10", "vp9", "av1"] {
            let Some((n, secs, last)) = run(f, 48) else { continue };
            let px = to_pixels(&last);
            let mean: f64 = px.iter().map(|p| p.g() as f64).sum::<f64>() / px.len() as f64;
            eprintln!("{f}: {n} pictures in {secs:.2}s = {:.1} fps, {}x{} {}bit {:?}, mean G {mean:.0}, last pts {:.3}", n as f64 / secs, last.w, last.h, last.bits, last.layout, last.pts);
            assert!(n >= 40, "{f}: only {n} pictures");
            assert!(mean > 20.0 && mean < 235.0);
            // dump for eyeballing
            let mut ppm = format!("P6\n{} {}\n255\n", last.w, last.h).into_bytes();
            ppm.extend(px.iter().flat_map(|p| [p.r(), p.g(), p.b()]));
            let _ = std::fs::write(format!("/tmp/claude-0/v/{f}.ppm"), ppm);
        }
    }
}
