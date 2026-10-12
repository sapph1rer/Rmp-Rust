//! Windows backend: Media Foundation Source Reader (+ D3D11/DXVA hardware decode).
//!
//! One thread owns the IMFSourceReader. It asks the reader for video converted
//! to RGB32 and audio converted to 32-bit float stereo, so we never touch codecs
//! ourselves. With a D3D11 device attached (IMFDXGIDeviceManager) Windows decodes
//! H.264/HEVC/VP9/AV1 on the GPU and the video processor does the colour
//! conversion on the GPU too; we only copy the final BGRA picture to the CPU.
//!
//! NOTE: written without access to a Windows compiler. If `cargo build` reports a
//! signature mismatch for a `windows` crate call, it is a one-line fix.

use crate::player::{add_external_sub, Cmd, Frame, MediaInfo, Options, Shared, TrackInfo};
use anyhow::{anyhow, bail, Result};
use std::ptr;
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use eframe::egui::Color32;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Com::StructuredStorage::{PropVariantToUInt64, PROPVARIANT};
use windows::Win32::System::Variant::VT_I8;

// IMFSourceReader stream selectors (values from mfreadwrite.h)
const FIRST_VIDEO: u32 = 0xFFFF_FFFC;
const ALL_STREAMS: u32 = 0xFFFF_FFFE;
const ANY_STREAM: u32 = 0xFFFF_FFFE;
const MEDIASOURCE: u32 = 0xFFFF_FFFF;
// MF_SOURCE_READER_FLAG bits
const F_ERROR: u32 = 0x1;
const F_EOS: u32 = 0x2;
const F_TYPE_CHANGED: u32 = 0x20;

const VIDEO_Q: usize = 8; // decoded-ahead pictures (queued + being converted); keep below player.rs FRAME_Q_MAX

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    mgr: IMFDXGIDeviceManager,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

fn make_gpu() -> Result<Gpu> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let ctx = ctx.ok_or_else(|| anyhow!("no D3D11 context"))?;
        // MF decodes on its own threads using this device
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.ok_or_else(|| anyhow!("no DXGI device manager"))?;
        let unk: windows::core::IUnknown = device.cast()?;
        mgr.ResetDevice(&unk, token)?;
        Ok(Gpu { device, ctx, mgr, staging: None })
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}


/// COM objects are free-threaded here (MTA); this lets us hand them to the worker thread.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}
impl<T> SendBox<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

struct VJob {
    sample: SendBox<IMFSample>,
    pts: f64,
    serial: u64,
    w: u32,
    h: u32,
    stride: Option<i32>,
}

fn boost_thread() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Converts decoded pictures (GPU readback + colour conversion + downscale) off the reader thread,
/// so a slow 4K picture can never starve the audio feed.
fn video_worker(rx: Receiver<VJob>, mut gpu: Option<Gpu>, sh: Arc<Shared>, inflight: Arc<AtomicUsize>, v_eos: Arc<AtomicBool>) {
    boost_thread();
    let mut hw_checked = false;
    while let Ok(job) = rx.recv() {
        if !sh.stale(job.serial) {
            let t = Instant::now();
            if let Some((w, h, pixels, on_gpu)) = sample_to_rgba(&job.sample.0, &mut gpu, job.w, job.h, job.stride) {
                let us = t.elapsed().as_micros() as u64;
                let old = sh.conv_us.load(Relaxed);
                sh.conv_us.store(if old == 0 { us } else { (old * 7 + us) / 8 }, Relaxed);
                if !hw_checked {
                    hw_checked = true;
                    sh.info.lock().unwrap().hw = if on_gpu { "d3d11/dxva".into() } else { "software".into() };
                }
                sh.push_frame(Frame { w, h, pixels, pts: job.pts, serial: job.serial });
            }
        }
        drop(job);
        inflight.fetch_sub(1, SeqCst);
        if v_eos.load(SeqCst) && inflight.load(SeqCst) == 0 {
            sh.video_eof.store(true, SeqCst);
        }
    }
}

fn video_codec_name(r: &IMFSourceReader, idx: u32) -> String {
    unsafe {
        let Ok(mt) = r.GetNativeMediaType(idx, 0) else { return "unknown codec".into() };
        let Ok(g) = mt.GetGUID(&MF_MT_SUBTYPE) else { return "unknown codec".into() };
        if g == MFVideoFormat_H264 {
            "H.264".into()
        } else if g == MFVideoFormat_HEVC {
            "HEVC / H.265".into()
        } else if g == MFVideoFormat_AV1 {
            "AV1".into()
        } else if g == MFVideoFormat_VP90 {
            "VP9".into()
        } else if g == MFVideoFormat_VP80 {
            "VP8".into()
        } else if g == MFVideoFormat_MPEG2 {
            "MPEG-2".into()
        } else if g == MFVideoFormat_WMV3 {
            "VC-1 / WMV".into()
        } else {
            let b = g.data1.to_le_bytes();
            if b.iter().all(|c| c.is_ascii_graphic()) {
                String::from_utf8_lossy(&b).into_owned()
            } else {
                format!("{:08X}", g.data1)
            }
        }
    }
}

struct Reader {
    r: IMFSourceReader,
    vidx: Option<u32>,
    aidx: Option<u32>,
    audio_streams: Vec<u32>,
    hw: bool,
    /// Windows has no decoder for the video stream (message to show if we cannot decode it ourselves)
    video_error: Option<String>,
    /// software AC-3/E-AC-3 decoder when Windows cannot decode the current audio stream
    pure: std::cell::RefCell<Option<crate::codecs::PureDecoder>>,
}

/// Create the reader; `gpu` = Some => hardware decode path.
fn open_reader(url: &str, gpu: Option<&Gpu>) -> Result<Reader> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 4)?;
        let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes"))?;
        if gpu.is_none() {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        }
        if let Some(g) = gpu {
            // GPU path: the advanced processor converts to RGB32 on the GPU (the plain one is CPU-only)
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            let unk: windows::core::IUnknown = g.mgr.cast()?;
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &unk)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        }
        let w = wide(url);
        let r = MFCreateSourceReaderFromURL(PCWSTR(w.as_ptr()), Some(&attrs))?;

        // find streams
        let mut vidx = None;
        let mut audio_streams = Vec::new();
        let mut i = 0u32;
        while let Ok(mt) = r.GetNativeMediaType(i, 0) {
            let major = mt.GetMajorType()?;
            if major == MFMediaType_Video && vidx.is_none() {
                vidx = Some(i);
            } else if major == MFMediaType_Audio {
                audio_streams.push(i);
            }
            i += 1;
            if i > 64 {
                break;
            }
        }
        if vidx.is_none() && audio_streams.is_empty() {
            bail!("no playable audio/video streams");
        }
        r.SetStreamSelection(ALL_STREAMS, false)?;
        let mut video_error = None;
        if let Some(v) = vidx {
            r.SetStreamSelection(v, true)?;
            let mt = MFCreateMediaType()?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            if let Err(e) = r.SetCurrentMediaType(v, None, &mt) {
                let name = video_codec_name(&r, v);
                let ext = if name.contains("HEVC") { "HEVC Video Extensions" } else if name.contains("AV1") { "AV1 Video Extension" } else if name.contains("VP9") { "VP9 Video Extensions" } else { "the matching video extension" };
                video_error = Some(format!("Windows has no decoder for this video ({name}). Install \"{ext}\" from the Microsoft Store (menu: Codec information). [{e}]"));
                let _ = r.SetStreamSelection(v, false);
            }
        }
        let vidx = if video_error.is_some() { None } else { vidx };
        Ok(Reader { r, vidx, aidx: None, audio_streams, hw: gpu.is_some(), video_error, pure: std::cell::RefCell::new(None) })
    }
}

fn set_audio_stream(rd: &mut Reader, idx: u32, rate: u32) -> Result<()> {
    unsafe {
        if let Some(old) = rd.aidx.take() {
            let _ = rd.r.SetStreamSelection(old, false);
        }
        *rd.pure.borrow_mut() = None;
        rd.r.SetStreamSelection(idx, true)?;
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
        mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)?;
        mt.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 8)?;
        mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 8)?;
        match rd.r.SetCurrentMediaType(idx, None, &mt) {
            Ok(()) => {}
            Err(e) => {
                // Windows has no Dolby decoder: take the compressed packets and decode them ourselves
                let name = audio_codec_name(rd, idx);
                let eac3 = name == "E-AC3";
                if name == "AC3" || eac3 {
                    let native = rd.r.GetNativeMediaType(idx, 0)?;
                    let in_rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48000);
                    rd.r.SetCurrentMediaType(idx, None, &native)?;
                    let kind = if eac3 { crate::codecs::Kind::Eac3 } else { crate::codecs::Kind::Ac3 };
                    match crate::codecs::PureDecoder::new(kind, &[], 2, in_rate, rate) {
                        Ok(d) => *rd.pure.borrow_mut() = Some(d),
                        Err(msg) => {
                            let _ = rd.r.SetStreamSelection(idx, false);
                            return Err(anyhow!("software {name} decoder failed to start: {msg}"));
                        }
                    }
                    logln!("audio stream {idx}: using built-in {name} decoder ({e})");
                } else {
                    let _ = rd.r.SetStreamSelection(idx, false);
                    return Err(e.into());
                }
            }
        }
        rd.aidx = Some(idx);
        Ok(())
    }
}

/// Stop the Media Foundation audio stream (used when an own-decoder track takes over).
fn clear_audio_stream(rd: &mut Reader) {
    unsafe {
        if let Some(old) = rd.aidx.take() {
            let _ = rd.r.SetStreamSelection(old, false);
        }
    }
    *rd.pure.borrow_mut() = None;
}

/// One selectable audio track: either a Media Foundation stream (`mf` = index into `audio_streams`)
/// or a track decoded by our own demuxer + pure-Rust decoder (`ext`, local Matroska only).
#[derive(Clone)]
struct AEntry {
    id: usize,
    mf: Option<usize>,
    ext: Option<crate::mkv::Track>,
    codec: String,
    channels: u32,
    lang: String,
    name: String,
    default: bool,
    playable: bool,
}

const EXT_BASE: usize = 1000;

fn mkv_family(codec: &str) -> &'static str {
    if codec.starts_with("A_AAC") {
        "AAC"
    } else if codec.starts_with("A_MPEG/L3") || codec.starts_with("A_MPEG/L2") {
        "MP3"
    } else if codec.starts_with("A_FLAC") {
        "FLAC"
    } else {
        ""
    }
}

fn ch_text(c: u32) -> String {
    match c {
        0 => String::new(),
        1 => " mono".into(),
        2 => " 2.0".into(),
        6 => " 5.1".into(),
        8 => " 7.1".into(),
        c => format!(" {c}ch"),
    }
}

fn entry_label(e: &AEntry, n: usize) -> String {
    let lang = crate::lang::name(&e.lang);
    let head = if !lang.is_empty() { lang } else { format!("Track {}", n + 1) };
    let mut label = format!("{head}  ({}{})", e.codec, ch_text(e.channels));
    let name = e.name.trim();
    if !name.is_empty() && !name.to_lowercase().contains(&head.to_lowercase()) {
        label.push_str(&format!(" - {name}"));
    }
    label
}

/// Make `e` the audio that plays: stops the previous own-decoder thread, switches MF streams.
fn activate_audio(sh: &Arc<Shared>, rd: &mut Reader, ext: &mut Option<crate::extaudio::ExtAudio>, e: &AEntry, path: &str, out_rate: u32, start: f64) -> Result<()> {
    if let Some(mut old) = ext.take() {
        old.stop();
    }
    if let Some(t) = &e.ext {
        clear_audio_stream(rd);
        let x = crate::extaudio::ExtAudio::spawn(sh.clone(), path, t, out_rate, start).map_err(|m| anyhow!("{} decoder: {m}", e.codec))?;
        *ext = Some(x);
        Ok(())
    } else if let Some(n) = e.mf {
        let id = rd.audio_streams[n];
        set_audio_stream(rd, id, out_rate)
    } else {
        bail!("no decoder for {}", e.codec)
    }
}

// ------------------------------------------------------------------ media / codec information

fn attr_string(a: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut n = 0u32;
        a.GetAllocatedString(key, &mut p, &mut n).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// (name, is_hardware) of the Windows decoders installed for a video subtype.
fn list_decoders(subtype: GUID) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    unsafe {
        let info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
        for (flags, hw) in [(MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT, false), (MFT_ENUM_FLAG_HARDWARE, true)] {
            let mut arr: *mut Option<IMFActivate> = ptr::null_mut();
            let mut n = 0u32;
            if MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, Some(&info), None, &mut arr, &mut n).is_err() || arr.is_null() {
                continue;
            }
            for i in 0..n as usize {
                if let Some(act) = ptr::read(arr.add(i)) {
                    let name = act.cast::<IMFAttributes>().ok().and_then(|a| attr_string(&a, &MFT_FRIENDLY_NAME_Attribute)).unwrap_or_else(|| "(unnamed decoder)".into());
                    if !out.iter().any(|(n, h): &(String, bool)| *n == name && *h == hw) {
                        out.push((name, hw));
                    }
                }
            }
            CoTaskMemFree(Some(arr as *const _));
        }
    }
    out
}

/// Name of the decoder transform the reader uses for a stream.
fn stream_decoder_name(rd: &Reader, idx: u32) -> Option<String> {
    unsafe {
        let mut p: *mut std::ffi::c_void = ptr::null_mut();
        rd.r.GetServiceForStream(idx, &GUID::zeroed(), &IMFTransform::IID, &mut p).ok()?;
        if p.is_null() {
            return None;
        }
        let t = IMFTransform::from_raw(p);
        let a = t.GetAttributes().ok()?;
        attr_string(&a, &MFT_FRIENDLY_NAME_Attribute)
    }
}

fn human_bytes(b: u64) -> String {
    let f = b as f64;
    if f >= 1e9 { format!("{:.2} GB", f / 1e9) } else if f >= 1e6 { format!("{:.1} MB", f / 1e6) } else { format!("{:.0} KB", f / 1e3) }
}

fn sw_codec_label(codec: &str) -> String {
    match crate::vcodecs::VKind::from_mkv(codec) {
        Some(crate::vcodecs::VKind::Hevc) => "HEVC / H.265".into(),
        Some(k) => k.name().to_string(),
        None => codec.to_string(),
    }
}

fn sw_decoder_name(t: &crate::mkv::Track) -> &'static str {
    match crate::vcodecs::VKind::from_mkv(&t.codec) {
        Some(crate::vcodecs::VKind::Hevc) => "oxideav-h265",
        Some(crate::vcodecs::VKind::Vp9) => "oxideav-vp9",
        _ => "rav1d",
    }
}

fn sw_video_details(t: &crate::mkv::Track) -> Vec<String> {
    let mut v = vec![format!("Codec:        {}", sw_codec_label(&t.codec))];
    if t.width > 0 {
        v.push(format!("Resolution:   {}x{}", t.width, t.height));
    }
    if t.default_duration_ns > 0 {
        v.push(format!("Frame rate:   {:.3} fps", 1e9 / t.default_duration_ns as f64));
    }
    v.push(format!("Decoder:      built-in software ({}), pure Rust", sw_decoder_name(t)));
    v
}

/// Rough guide to what the pure-Rust decoders manage on a typical PC.
fn slow_warning(k: crate::vcodecs::VKind, w: u32, h: u32) -> Option<String> {
    use crate::vcodecs::VKind::*;
    let px = w as u64 * h as u64;
    let slow = match k {
        Hevc => px > 854 * 480,
        Vp9 => px > 1280 * 720,
        Av1 => px > 1920 * 1080,
    };
    slow.then(|| format!("Software {} decoding at {w}x{h} may stutter - install the video extension from the Microsoft Store (Codec information)", k.name()))
}

fn video_details(rd: &Reader) -> Vec<String> {
    let mut v = Vec::new();
    unsafe {
        let Some(idx) = rd.vidx else { return v };
        let Ok(mt) = rd.r.GetNativeMediaType(idx, 0) else { return v };
        let codec = video_codec_name(&rd.r, idx);
        let sub = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        let prof = mt.GetUINT32(&MF_MT_MPEG2_PROFILE).ok();
        let lvl = mt.GetUINT32(&MF_MT_MPEG2_LEVEL).ok();
        let mut extra = String::new();
        if sub == MFVideoFormat_H264 {
            let p = match prof { Some(66) => "Baseline", Some(77) => "Main", Some(88) => "Extended", Some(100) => "High", Some(110) => "High 10", Some(122) => "High 4:2:2", Some(244) => "High 4:4:4", _ => "" };
            if !p.is_empty() { extra.push_str(p); }
            if let Some(l) = lvl { extra.push_str(&format!(" L{:.1}", l as f32 / 10.0)); }
        } else if sub == MFVideoFormat_HEVC {
            let p = match prof { Some(1) => "Main", Some(2) => "Main 10", Some(3) => "Main Still", _ => "" };
            if !p.is_empty() { extra.push_str(p); }
            if let Some(l) = lvl { extra.push_str(&format!(" L{:.1}", l as f32 / 30.0)); }
        }
        v.push(format!("Codec:        {codec}{}", if extra.trim().is_empty() { String::new() } else { format!(" ({})", extra.trim()) }));
        if let Ok(sz) = mt.GetUINT64(&MF_MT_FRAME_SIZE) {
            v.push(format!("Resolution:   {}x{}", sz >> 32, sz & 0xFFFF_FFFF));
        }
        if let Ok(r) = mt.GetUINT64(&MF_MT_FRAME_RATE) {
            let (n, d) = ((r >> 32) as f64, (r & 0xFFFF_FFFF) as f64);
            if d > 0.0 && n > 0.0 { v.push(format!("Frame rate:   {:.3} fps", n / d)); }
        }
        if let Ok(b) = mt.GetUINT32(&MF_MT_AVG_BITRATE) {
            if b > 0 { v.push(format!("Bitrate:      {:.1} Mbps", b as f64 / 1e6)); }
        }
        let tf = mt.GetUINT32(&MF_MT_TRANSFER_FUNCTION).unwrap_or(0);
        let prim = mt.GetUINT32(&MF_MT_VIDEO_PRIMARIES).unwrap_or(0);
        if tf == 15 {
            v.push("HDR:          HDR10 / PQ (BT.2020) - shown without tone-mapping".into());
        } else if tf == 16 {
            v.push("HDR:          HLG - shown without tone-mapping".into());
        } else if prim == 9 {
            v.push("Colour:       BT.2020".into());
        }
    }
    v
}

/// Builds the texts of the "Media information" and "Codec information" windows.
/// Returns (media_text, codec_text, video codecs missing on this PC).
fn build_reports(rd: &Reader, entries: &[AEntry], src_name: &str, is_http: bool, container: &str, size: Option<u64>, duration: Option<f64>, hw: bool, active_ext: bool, sw: Option<&crate::mkv::Track>) -> (String, String, Vec<String>) {
    let mut m = String::new();
    m.push_str(&format!("File:         {src_name}\n"));
    m.push_str(&format!("Source:       {}\n", if is_http { "link (streamed)" } else { "local file" }));
    m.push_str(&format!("Container:    {container}\n"));
    if let Some(s) = size { m.push_str(&format!("Size:         {}\n", human_bytes(s))); }
    if let Some(d) = duration {
        let t = d as u64;
        m.push_str(&format!("Duration:     {}:{:02}:{:02}\n", t / 3600, t / 60 % 60, t % 60));
    }
    m.push_str("\nVIDEO\n");
    let vd = match sw {
        Some(t) => sw_video_details(t),
        None => video_details(rd),
    };
    if vd.is_empty() { m.push_str("  (no video)\n"); }
    for l in &vd { m.push_str(&format!("  {l}\n")); }
    m.push_str("\nAUDIO\n");
    if entries.is_empty() { m.push_str("  (no audio)\n"); }
    for (n, e) in entries.iter().enumerate() {
        let rate = match (&e.ext, e.mf) {
            (Some(t), _) if t.sample_rate > 0.0 => format!("{:.0} Hz", t.sample_rate),
            (_, Some(i)) => unsafe { rd.r.GetNativeMediaType(rd.audio_streams[i], 0).ok().and_then(|mt| mt.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()).map(|r| format!("{r} Hz")).unwrap_or_default() },
            _ => String::new(),
        };
        let lang = crate::lang::name(&e.lang);
        m.push_str(&format!("  {}. {:<10} {}{}  {}  {}{}\n", n + 1, if lang.is_empty() { "-".into() } else { lang }, e.codec, ch_text(e.channels), rate, if e.default { "[default] " } else { "" }, if e.name.trim().is_empty() { String::new() } else { format!("\"{}\"", e.name.trim()) }));
    }

    // codecs
    let mut c = String::new();
    c.push_str("VIDEO DECODER IN USE\n");
    match rd.vidx {
        Some(v) => {
            let name = stream_decoder_name(rd, v).unwrap_or_else(|| "Windows Media Foundation (name not reported)".into());
            c.push_str(&format!("  {name}\n  {}\n", if hw { "GPU decoding (D3D11 / DXVA) is on" } else { "software decoding (CPU)" }));
        }
        None => c.push_str("  (no video)\n"),
    }
    c.push_str("\nAUDIO DECODERS\n");
    for (n, e) in entries.iter().enumerate() {
        let how = if e.ext.is_some() {
            match e.codec.as_str() {
                "AC3" | "E-AC3" => "built-in, pure Rust (oxideav-ac3)",
                "DTS" => "built-in, pure Rust (oxideav-dts, core stream)",
                "TrueHD" => "built-in, pure Rust (truehd)",
                "Opus" => "built-in, pure Rust (opus-decoder)",
                "Vorbis" => "built-in, pure Rust (symphonia-codec-vorbis)",
                _ => "built-in",
            }
        } else {
            "Windows Media Foundation"
        };
        c.push_str(&format!("  Track {}: {:<8} -> {how}{}\n", n + 1, e.codec, if e.playable { "" } else { "   [cannot be decoded]" }));
    }
    let _ = active_ext;
    c.push_str("\nWINDOWS VIDEO DECODERS ON THIS PC\n");
    let mut missing = Vec::new();
    let table: [(&str, &str, GUID); 6] = [
        ("H.264", "", MFVideoFormat_H264),
        ("HEVC / H.265", "HEVC", MFVideoFormat_HEVC),
        ("VP9", "VP9", MFVideoFormat_VP90),
        ("AV1", "AV1", MFVideoFormat_AV1),
        ("MPEG-2", "", MFVideoFormat_MPEG2),
        ("VC-1", "", MFVideoFormat_WVC1),
    ];
    for (label, key, sub) in table {
        let d = list_decoders(sub);
        if d.is_empty() {
            c.push_str(&format!("  {label:<13} NOT INSTALLED{}\n", if key.is_empty() { String::new() } else { format!("  (get the \"{key}\" video extension from the Microsoft Store)") }));
            if !key.is_empty() { missing.push(key.to_string()); }
        } else {
            let list: Vec<String> = d.iter().map(|(n, h)| format!("{n} [{}]", if *h { "GPU" } else { "CPU" })).collect();
            c.push_str(&format!("  {label:<13} {}\n", list.join("; ")));
        }
    }
    c.push_str("\nBUILT IN (no Windows codec needed)\n  Video: HEVC, VP9, AV1 in Matroska/WebM, used only when Windows has no decoder (software, slower than the Store extensions)\n  Audio: AC-3, E-AC-3, DTS (core), TrueHD, Opus, Vorbis (Matroska files and .mkv links)\n  Subtitles: SRT, ASS/SSA, WebVTT; shaped text rendering (rustybuzz)\n");
    (m, c, missing)
}

fn audio_channels(rd: &Reader, idx: u32) -> u32 {
    unsafe {
        rd.r.GetNativeMediaType(idx, 0)
            .ok()
            .and_then(|mt| mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok())
            .unwrap_or(0)
    }
}

fn audio_codec_name(rd: &Reader, idx: u32) -> String {
    unsafe {
        let Ok(mt) = rd.r.GetNativeMediaType(idx, 0) else { return "unknown".into() };
        let Ok(g) = mt.GetGUID(&MF_MT_SUBTYPE) else { return "unknown".into() };
        if g == MFAudioFormat_Dolby_DDPlus {
            "E-AC3".into()
        } else if g == MFAudioFormat_Dolby_AC3 {
            "AC3".into()
        } else if g == MFAudioFormat_DTS {
            "DTS".into()
        } else if g == MFAudioFormat_Opus {
            "Opus".into()
        } else if g == MFAudioFormat_AAC {
            "AAC".into()
        } else if g == MFAudioFormat_MP3 {
            "MP3".into()
        } else if g == MFAudioFormat_FLAC {
            "FLAC".into()
        } else if g.data1 == 0xEB27CEC4 {
            "TrueHD".into()
        } else {
            format!("{:08X}", g.data1)
        }
    }
}

fn video_size(rd: &Reader) -> (u32, u32) {
    unsafe {
        let Some(v) = rd.vidx else { return (0, 0) };
        if let Ok(mt) = rd.r.GetCurrentMediaType(v) {
            if let Ok(sz) = mt.GetUINT64(&MF_MT_FRAME_SIZE) {
                return ((sz >> 32) as u32, (sz & 0xFFFF_FFFF) as u32);
            }
        }
        (0, 0)
    }
}

fn default_stride(rd: &Reader) -> Option<i32> {
    unsafe {
        let v = rd.vidx?;
        let mt = rd.r.GetCurrentMediaType(v).ok()?;
        mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok().map(|s| s as i32)
    }
}

struct SrcPtr(*const u8);
unsafe impl Send for SrcPtr {}
unsafe impl Sync for SrcPtr {}
impl SrcPtr {
    fn get(&self) -> *const u8 {
        self.0
    }
}

/// Integer downscale factor so pictures stay around 1080p: 4K -> 2, 1440p/1080p -> 1.
/// Cap on the picture height handed to the UI (0 = native); set from the settings when playback starts.
static MAX_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1080);

fn decimation(w: usize, h: usize) -> usize {
    let lim = MAX_H.load(Relaxed) as f32;
    if lim < 1.0 {
        return 1;
    }
    let f = (w as f32 / (lim * 16.0 / 9.0)).max(h as f32 / lim);
    (f.round() as usize).max(1)
}

/// BGRX rows (any pitch / orientation) -> egui pixels (opaque), downscaled by an integer factor
/// (2x2 average for 2, nearest beyond) and converted on several threads. Returns (w, h, pixels).
fn bgrx_to_pixels(w: usize, h: usize, base: *const u8, pitch: isize) -> (usize, usize, Vec<Color32>) {
    let n = decimation(w, h);
    let (ow, oh) = (w / n, h / n);
    let mut out = vec![Color32::BLACK; ow * oh];
    if ow == 0 || oh == 0 {
        return (ow, oh, out);
    }
    let src = SrcPtr(base);
    let threads = if ow * oh > 300_000 { std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8) } else { 1 };
    let rows_per = (oh + threads - 1) / threads;
    std::thread::scope(|s| {
        for (bi, band) in out.chunks_mut(rows_per * ow).enumerate() {
            let src = &src;
            s.spawn(move || {
                let base = src.get();
                for (ry, row) in band.chunks_mut(ow).enumerate() {
                    let y = (bi * rows_per + ry) * n;
                    unsafe {
                        let r0 = std::slice::from_raw_parts(base.offset(y as isize * pitch), w * 4);
                        match n {
                            1 => {
                                for (px, s) in row.iter_mut().zip(r0.chunks_exact(4)) {
                                    *px = Color32::from_rgb(s[2], s[1], s[0]);
                                }
                            }
                            2 => {
                                let r1 = std::slice::from_raw_parts(base.offset((y + 1) as isize * pitch), w * 4);
                                for (x, px) in row.iter_mut().enumerate() {
                                    let i = x * 8;
                                    let c = |k: usize| (r0[i + k] as u32 + r0[i + 4 + k] as u32 + r1[i + k] as u32 + r1[i + 4 + k] as u32 + 2) / 4;
                                    *px = Color32::from_rgb(c(2) as u8, c(1) as u8, c(0) as u8);
                                }
                            }
                            _ => {
                                for (x, px) in row.iter_mut().enumerate() {
                                    let i = x * n * 4;
                                    *px = Color32::from_rgb(r0[i + 2], r0[i + 1], r0[i]);
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    (ow, oh, out)
}

/// Convert a decoded video sample to an RGBA picture. `pw`/`ph`/`stride` describe the picture for
/// system-memory samples (and crop GPU surfaces). Returns (w, h, pixels, was_gpu_surface).
fn sample_to_rgba(sample: &IMFSample, gpu: &mut Option<Gpu>, pw: u32, ph: u32, stride: Option<i32>) -> Option<(u32, u32, Vec<Color32>, bool)> {
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().ok()?;
        // GPU surface?
        if let (Ok(dxgi), Some(g)) = (buf.cast::<IMFDXGIBuffer>(), gpu.as_mut()) {
            let mut raw: *mut std::ffi::c_void = ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).ok()?;
            if raw.is_null() {
                return None;
            }
            let tex = ID3D11Texture2D::from_raw(raw);
            let sub = dxgi.GetSubresourceIndex().unwrap_or(0);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            let need_new = !matches!(&g.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
            if need_new {
                let mut d = desc;
                d.Usage = D3D11_USAGE_STAGING;
                d.BindFlags = 0;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                d.MiscFlags = 0;
                d.MipLevels = 1;
                d.ArraySize = 1;
                let mut st: Option<ID3D11Texture2D> = None;
                g.device.CreateTexture2D(&d, None, Some(&mut st)).ok()?;
                g.staging = Some((st?, desc.Width, desc.Height));
            }
            let staging = &g.staging.as_ref()?.0;
            g.ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, &tex, sub, None);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            g.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            // the texture can be padded; show only the real picture
            let (w, h) = if pw > 0 && ph > 0 && pw <= desc.Width && ph <= desc.Height { (pw, ph) } else { (desc.Width, desc.Height) };
            let (ow, oh, rgba) = bgrx_to_pixels(w as usize, h as usize, m.pData as *const u8, m.RowPitch as isize);
            g.ctx.Unmap(staging, 0);
            return Some((ow as u32, oh as u32, rgba, true));
        }
        // system-memory picture
        let (w, h) = (pw, ph);
        if w == 0 || h == 0 {
            return None;
        }
        if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
            let mut scan0: *mut u8 = ptr::null_mut();
            let mut pitch = 0i32;
            b2.Lock2D(&mut scan0, &mut pitch).ok()?;
            let (ow, oh, rgba) = bgrx_to_pixels(w as usize, h as usize, scan0, pitch as isize);
            let _ = b2.Unlock2D();
            return Some((ow as u32, oh as u32, rgba, false));
        }
        let mut data: *mut u8 = ptr::null_mut();
        let (mut max, mut cur) = (0u32, 0u32);
        buf.Lock(&mut data, Some(&mut max), Some(&mut cur)).ok()?;
        let stride = stride.unwrap_or((w * 4) as i32);
        let (ow, oh, rgba) = if stride < 0 {
            // bottom-up DIB: first row in memory is the last picture row
            let last = data.offset(((h as isize) - 1) * (-stride) as isize);
            bgrx_to_pixels(w as usize, h as usize, last, stride as isize)
        } else {
            bgrx_to_pixels(w as usize, h as usize, data, stride as isize)
        };
        let _ = buf.Unlock();
        Some((ow as u32, oh as u32, rgba, false))
    }
}

fn sample_to_pcm(sample: &IMFSample) -> Option<Vec<f32>> {
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().ok()?;
        let mut data: *mut u8 = ptr::null_mut();
        let (mut max, mut cur) = (0u32, 0u32);
        buf.Lock(&mut data, Some(&mut max), Some(&mut cur)).ok()?;
        let n = cur as usize / 4;
        let mut v = vec![0f32; n];
        ptr::copy_nonoverlapping(data as *const f32, v.as_mut_ptr(), n);
        let _ = buf.Unlock();
        Some(v)
    }
}

fn sample_to_bytes(sample: &IMFSample) -> Option<Vec<u8>> {
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().ok()?;
        let mut data: *mut u8 = ptr::null_mut();
        let (mut max, mut cur) = (0u32, 0u32);
        buf.Lock(&mut data, Some(&mut max), Some(&mut cur)).ok()?;
        let v = std::slice::from_raw_parts(data, cur as usize).to_vec();
        let _ = buf.Unlock();
        Some(v)
    }
}

fn duration_secs(rd: &Reader) -> Option<f64> {
    unsafe {
        let pv = rd.r.GetPresentationAttribute(MEDIASOURCE, &MF_PD_DURATION).ok()?;
        let hns = PropVariantToUInt64(&pv).ok()?;
        if hns == 0 { None } else { Some(hns as f64 / 1e7) }
    }
}

fn seek_reader(rd: &Reader, secs: f64) {
    unsafe {
        let hns = (secs.max(0.0) * 1e7) as i64;
        let mut pv = PROPVARIANT::default();
        (*pv.Anonymous.Anonymous).vt = VT_I8;
        (*pv.Anonymous.Anonymous).Anonymous.hVal = hns;
        let _ = rd.r.Flush(ALL_STREAMS);
        if let Err(e) = rd.r.SetCurrentPosition(&GUID::zeroed(), &pv) {
            logln!("seek failed: {e}");
        }
    }
}

/// Same-name .srt/.ass/.vtt next to a local file (like mpv's sub-auto=fuzzy, exact stem only).
fn sidecar_subs(url: &str) -> Vec<String> {
    let p = std::path::Path::new(url);
    if url.contains("://") || !p.is_file() {
        return vec![];
    }
    let (Some(dir), Some(stem)) = (p.parent(), p.file_stem().and_then(|s| s.to_str())) else { return vec![] };
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with(stem) && crate::subs::is_sub_ext(name) {
                v.push(path.to_string_lossy().into_owned());
            }
        }
    }
    v.sort();
    v
}

pub fn run(sh: Arc<Shared>, url: String, opts: Options, cmd_rx: Receiver<Cmd>) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let r = run_inner(&sh, url, opts, cmd_rx);
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    r
}

fn run_inner(sh: &Arc<Shared>, url: String, opts: Options, cmd_rx: Receiver<Cmd>) -> Result<()> {
    // Links that need headers (referrer/UA/cookies) or are HLS playlists go through the
    // local proxy (net.rs), because Media Foundation cannot send custom HTTP headers.
    let orig_url = url.clone();
    let mut url = url;
    let low = url.to_lowercase();
    let path_low = low.split('?').next().unwrap_or("");
    let is_http = low.starts_with("http://") || low.starts_with("https://");
    let needs_proxy = is_http
        && (opts.referrer.is_some()
            || opts.user_agent.is_some()
            || !opts.headers.is_empty()
            || opts.force_format.as_deref() == Some("hls")
            || path_low.ends_with(".m3u8")
            || path_low.ends_with(".txt"));
    if needs_proxy {
        match crate::net::serve(&url, &opts, sh.quit.clone(), false) {
            Ok(local) => url = local,
            Err(e) => return Err(anyhow!("cannot open link: {e:#}")),
        }
    }
    MAX_H.store(opts.max_height, Relaxed);
    let want_hw = !matches!(opts.hwdec.trim().to_lowercase().as_str(), "no" | "none" | "off" | "false" | "software");
    let mut gpu = if want_hw { make_gpu().map_err(|e| logln!("GPU init failed ({e}), using software decode")).ok() } else { None };

    let mut rd = match open_reader(&url, gpu.as_ref()) {
        Ok(r) => r,
        Err(e) if gpu.is_some() => {
            logln!("hardware path failed ({e}), retrying in software");
            gpu = None;
            open_reader(&url, None).map_err(|e| anyhow!("cannot open '{url}': {e}"))?
        }
        Err(e) => return Err(anyhow!("cannot open '{url}': {e}")),
    };

    // audio tracks: Media Foundation streams + (local Matroska) tracks we decode ourselves
    // Matroska we can also read ourselves: local files and direct .mkv links (HTTP Range requests;
    // `url` is the loopback proxy address when headers are needed)
    let mkv_src = if is_http { url.clone() } else { orig_url.clone() };
    let local_mkv = if is_http {
        path_low.ends_with(".mkv") || path_low.ends_with(".mka") || path_low.ends_with(".webm")
    } else {
        crate::mkv::is_matroska(&orig_url)
    } && crate::mkv::is_matroska(&mkv_src);
    let (mkv_all, mkv_dur): (Vec<crate::mkv::Track>, f64) = if local_mkv { crate::mkv::read_tracks_duration(&mkv_src).unwrap_or_default() } else { Default::default() };
    let mkv_audio: Vec<crate::mkv::Track> = mkv_all.iter().filter(|t| t.ttype == 2).cloned().collect();
    // video Windows cannot decode: HEVC / VP9 / AV1 in Matroska go to the built-in software decoders
    let sw_track: Option<crate::mkv::Track> = match rd.video_error.as_ref() {
        None => None,
        Some(err) => match mkv_all.iter().find(|t| t.ttype == 1 && crate::vcodecs::VKind::from_mkv(&t.codec).is_some()) {
            Some(t) => {
                logln!("{err} -> using the built-in {} decoder", t.codec);
                Some(t.clone())
            }
            None => return Err(anyhow!("cannot open '{url}': {err}")),
        },
    };
    // audio output device
    let mut out_rate = 0;
    let mut audio_note = String::new();
    if !rd.audio_streams.is_empty() || !mkv_audio.is_empty() {
        match crate::player::start_audio_output(sh.clone()) {
            Some(r) => out_rate = r,
            None => {
                logln!("no audio output device");
                audio_note = "No audio output device".into();
            }
        }
    } else {
        audio_note = "No audio track in this file".into();
    }
    let mf_names: Vec<String> = rd.audio_streams.iter().map(|id| audio_codec_name(&rd, *id)).collect();
    let mut entries: Vec<AEntry> = Vec::new();
    let mut used = vec![false; mf_names.len()];
    for t in &mkv_audio {
        let mk = |id: usize, mf: Option<usize>, ext: Option<crate::mkv::Track>, codec: String, ch: u32| AEntry {
            id, mf, ext, codec, channels: ch, lang: t.lang.clone(), name: t.name.clone(), default: t.default, playable: false,
        };
        if let Some(kind) = crate::codecs::Kind::from_mkv(&t.codec) {
            entries.push(mk(EXT_BASE + t.number as usize, None, Some(t.clone()), kind.name().into(), t.channels as u32));
            continue;
        }
        let fam = mkv_family(&t.codec);
        let pos = (0..mf_names.len()).find(|&n| !used[n] && (mf_names[n] == fam || (fam.is_empty() && !matches!(mf_names[n].as_str(), "AAC" | "MP3" | "FLAC"))));
        match pos {
            Some(n) => {
                used[n] = true;
                let ch = audio_channels(&rd, rd.audio_streams[n]).max(t.channels as u32);
                entries.push(mk(rd.audio_streams[n] as usize, Some(n), None, mf_names[n].clone(), ch));
            }
            None => entries.push(mk(EXT_BASE + t.number as usize, None, None, t.codec.trim_start_matches("A_").to_string(), t.channels as u32)),
        }
    }
    if mkv_audio.is_empty() {
        for n in 0..mf_names.len() {
            let ch = audio_channels(&rd, rd.audio_streams[n]);
            entries.push(AEntry { id: rd.audio_streams[n] as usize, mf: Some(n), ext: None, codec: mf_names[n].clone(), channels: ch, lang: String::new(), name: String::new(), default: false, playable: false });
        }
    }
    let mut ext: Option<crate::extaudio::ExtAudio> = None;
    let mut cur_id: Option<usize> = None;
    if out_rate > 0 {
        // probe every Media Foundation stream; own-decoder tracks are playable when a codec exists for them
        for e in entries.iter_mut() {
            if let Some(n) = e.mf {
                let id = rd.audio_streams[n];
                match set_audio_stream(&mut rd, id, out_rate) {
                    Ok(()) => e.playable = true,
                    Err(err) => logln!("audio stream {id} ({}) unusable: {err}", e.codec),
                }
            } else {
                e.playable = e.ext.is_some();
            }
        }
        clear_audio_stream(&mut rd);
        // language preference, then the file's default flag, then the first one that works
        let prefs = &opts.audio_langs;
        let pick = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.playable)
            .min_by_key(|(i, e)| (crate::lang::pref_rank(&e.lang, prefs).unwrap_or(usize::MAX), !e.default, *i))
            .map(|(i, _)| i);
        let mut active = false;
        if let Some(i) = pick {
            let order: Vec<usize> = std::iter::once(i).chain((0..entries.len()).filter(|&j| j != i && entries[j].playable)).collect();
            for j in order {
                let e = entries[j].clone();
                match activate_audio(sh, &mut rd, &mut ext, &e, &mkv_src, out_rate, opts.start) {
                    Ok(()) => {
                        active = true;
                        cur_id = Some(e.id);
                        break;
                    }
                    Err(err) => {
                        logln!("audio track {} failed: {err:#}", e.id);
                        entries[j].playable = false;
                    }
                }
            }
        }
        if !active {
            let bad: Vec<String> = entries.iter().map(|e| e.codec.clone()).collect();
            audio_note = format!("No sound: cannot decode this audio ({})", bad.join(", "));
            out_rate = 0;
        }
    }
    let cur_audio_id = cur_id;
    sh.has_audio.store(out_rate > 0, SeqCst);
    sh.has_video.store(rd.vidx.is_some() || sw_track.is_some(), SeqCst);
    sh.audio.lock().unwrap().rate = out_rate.max(1);
    sh.audio_delay.store(opts.audio_delay);

    // subtitles (external files + same-name sidecars)
    for p in sidecar_subs(&orig_url).into_iter().chain(opts.external_subs.iter().cloned()) {
        if let Err(e) = add_external_sub(sh, &p) {
            logln!("subtitle '{p}': {e:#}");
        }
    }

    // subtitle tracks embedded in a local MKV (read by our own parser; MF does not expose them)
    // (links: the subtitle blocks are spread over the whole file, so only files up to 256 MB are scanned)
    let scan_subs = local_mkv && (!is_http || crate::mkv::source_len(&mkv_src).map_or(false, |l| l <= 256 * 1024 * 1024));
    if local_mkv && !scan_subs {
        logln!("embedded subtitles are not read from large links; use --sub-file or a sidecar file");
    }
    if scan_subs {
        let (st, quit, path, sub_langs, shf) = (sh.subs.clone(), sh.quit.clone(), mkv_src.clone(), opts.sub_langs.clone(), sh.clone());
        std::thread::Builder::new().name("mkv-subs".into()).spawn(move || {
            let st2 = st.clone();
            let st3 = st.clone();
            st.lock().unwrap().loading = true;
            crate::mkv::scan(
                &path,
                &quit,
                || shf.position(),
                |tracks| {
                    let mut s = st.lock().unwrap();
                    for t in tracks {
                        s.add_track(20_000 + t.number as usize, t.label());
                        s.set_priority(20_000 + t.number as usize, crate::lang::priority(&t.lang));
                    }
                    if s.active.is_none() {
                        // language preference first, then the file's default track, then the first one
                        let pick = tracks
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| !t.forced)
                            .min_by_key(|(i, t)| (crate::lang::pref_rank(&t.lang, &sub_langs).unwrap_or(usize::MAX), !t.default, *i))
                            .map(|(_, t)| t)
                            .or_else(|| tracks.first());
                        s.active = pick.map(|t| 20_000 + t.number as usize);
                    }
                },
                move |batch| {
                    let mut s = st2.lock().unwrap();
                    for (n, c) in batch {
                        s.push_cue(20_000 + n as usize, c);
                    }
                },
            );
            st3.lock().unwrap().loading = false;
        })?;
    }

    let (w, h) = match &sw_track {
        Some(t) => (t.width, t.height),
        None => video_size(&rd),
    };
    let dur = duration_secs(&rd).or(if mkv_dur > 0.0 { Some(mkv_dur) } else { None });
    if let Some(t) = &sw_track {
        if let Some(k) = crate::vcodecs::VKind::from_mkv(&t.codec) {
            if let Some(msg) = slow_warning(k, w, h) {
                if !audio_note.is_empty() {
                    audio_note.push_str("; ");
                }
                audio_note.push_str(&msg);
                logln!("{msg}");
            }
        }
    }
    let title = opts.title.clone().unwrap_or_else(|| orig_url.rsplit(['/', '\\']).next().unwrap_or(&orig_url).split('?').next().unwrap_or("").to_string());
    let container = match path_low.rsplit('.').next().unwrap_or("") {
        "mkv" | "mka" => "Matroska".to_string(),
        "webm" => "WebM".into(),
        "mp4" | "m4v" | "m4a" => "MP4".into(),
        "mov" => "QuickTime".into(),
        "ts" | "m2ts" | "mts" => "MPEG transport stream".into(),
        "avi" => "AVI".into(),
        "wmv" | "asf" => "ASF / WMV".into(),
        "flv" => "Flash video".into(),
        "m3u8" | "txt" => "HLS (HTTP Live Streaming)".into(),
        e if opts.force_format.as_deref() == Some("hls") => format!("HLS ({e})"),
        e if e.is_empty() || e.len() > 5 => "unknown".into(),
        e => e.to_uppercase(),
    };
    let file_size = if is_http { if local_mkv { crate::mkv::source_len(&mkv_src) } else { None } } else { std::fs::metadata(&orig_url).ok().map(|m| m.len()) };
    let (media_text, codec_text, missing_codecs) = build_reports(&rd, &entries, &title, is_http, &container, file_size, dur, rd.hw, ext.is_some(), sw_track.as_ref());
    {
        let mut i = sh.info.lock().unwrap();
        *i = MediaInfo {
            loaded: true,
            title,
            duration: dur,
            width: w,
            height: h,
            vcodec: match &sw_track {
                Some(t) => sw_codec_label(&t.codec),
                None => rd.vidx.map(|v| video_codec_name(&rd.r, v)).unwrap_or_default(),
            },
            hw: if sw_track.is_some() { format!("software (built-in {} decoder)", sw_decoder_name(sw_track.as_ref().unwrap())) } else if rd.hw { "d3d11/dxva".into() } else { "software".into() },
            audio: entries.iter().enumerate().map(|(n, e)| TrackInfo { id: e.id, label: entry_label(e, n), playable: e.playable }).collect(),
            cur_audio: cur_audio_id,
            has_video: rd.vidx.is_some() || sw_track.is_some(),
            audio_note,
            media_text,
            codec_text,
            missing_codecs,
        };
    }

    // picture conversion runs on its own thread
    let inflight = Arc::new(AtomicUsize::new(0));
    let v_eos = Arc::new(AtomicBool::new(false));
    let (vtx, vrx): (Sender<VJob>, Receiver<VJob>) = std::sync::mpsc::channel();
    {
        let gpu_box = SendBox(gpu.take());
        let (sh2, inflight2, v_eos2) = (sh.clone(), inflight.clone(), v_eos.clone());
        std::thread::Builder::new().name("video-convert".into()).spawn(move || {
            let gpu = gpu_box.into_inner();
            video_worker(vrx, gpu, sh2, inflight2, v_eos2);
        })?;
    }
    boost_thread();
    let mut vsize = video_size(&rd);
    let mut vstride = default_stride(&rd);
    let mut last_stat = Instant::now();
    let mut last_counts = (0u64, 0u64);

    let mut t0 = 0.0f64; // timeline offset (first timestamp), so the UI starts at 0
    let mut t0_set = false;
    // built-in video decoder thread; its timestamps are the Matroska ones, so the timeline starts at 0
    let sw: Option<crate::swvideo::SwVideo> = match &sw_track {
        Some(t) => {
            t0_set = true;
            sh.t0.store(0.0);
            sh.t0_ready.store(true, SeqCst);
            Some(crate::swvideo::SwVideo::spawn(sh.clone(), &mkv_src, t, opts.start).map_err(|e| anyhow!("cannot start the {} decoder: {e}", t.codec))?)
        }
        None => None,
    };
    let mut serial = sh.serial.load(SeqCst);
    let (mut v_eof, mut a_eof) = (rd.vidx.is_none(), rd.aidx.is_none());

    let seek_at = std::cell::Cell::new(Instant::now());
    let sw_ref = sw.as_ref();
    let do_seek = |sh: &Arc<Shared>, rd: &Reader, ext: Option<&crate::extaudio::ExtAudio>, t: f64, t0: f64, serial: &mut u64, v_eof: &mut bool, a_eof: &mut bool| {
        let t = t.max(0.0);
        seek_reader(rd, t + t0);
        if let Some(d) = rd.pure.borrow_mut().as_mut() {
            d.reset();
        }
        if let Some(x) = ext {
            x.set_target(t); // must precede the serial bump below
        }
        if let Some(v) = sw_ref {
            v.set_target(t); // must precede the serial bump below
        }
        seek_at.set(Instant::now());
        *serial = sh.serial.fetch_add(1, SeqCst) + 1;
        sh.req_pos.store(f64::NAN);
        sh.seek_target.store(t);
        sh.frames.lock().unwrap().clear();
        sh.frames_cv.notify_all();
        {
            let mut a = sh.audio.lock().unwrap();
            a.q.clear();
            a.has_clock = false;
        }
        sh.audio_cv.notify_all();
        {
            let mut c = sh.clock.lock().unwrap();
            c.running = false;
            c.base_pts = t;
        }
        *v_eof = rd.vidx.is_none();
        *a_eof = rd.aidx.is_none();
        v_eos.store(false, SeqCst);
        sh.video_eof.store(*v_eof && sw_ref.is_none(), SeqCst);
        sh.audio_eof.store(*a_eof && ext.is_none(), SeqCst);
        sh.shown_serial.store(0, Relaxed);
    };

    if opts.start > 0.5 {
        do_seek(sh, &rd, ext.as_ref(), opts.start, t0, &mut serial, &mut v_eof, &mut a_eof);
    }

    let audio_cap = (out_rate as usize * 2) * 3 / 2; // 1.5 s of audio ahead; push_audio caps at 2 s
    let mut errors = 0;
    let mut pending_cmd: Option<Cmd> = None;
    loop {
        if sh.quit.load(Relaxed) {
            break;
        }
        // commands
        let idle = v_eof && a_eof;
        let cmd = if let Some(c) = pending_cmd.take() {
            Some(c)
        } else if idle {
            let c = cmd_rx.recv_timeout(Duration::from_millis(40)).ok();
            if c.is_some() {
                sh.cmd_pending.fetch_sub(1, SeqCst);
            }
            c
        } else {
            let c = cmd_rx.try_recv().ok();
            if c.is_some() {
                sh.cmd_pending.fetch_sub(1, SeqCst);
            }
            c
        };
        if let Some(c) = cmd {
            match c {
                Cmd::Seek(mut t) => {
                    // coalesce a burst of seeks (key repeat / button mashing) into the last one
                    while let Ok(next) = cmd_rx.try_recv() {
                        sh.cmd_pending.fetch_sub(1, SeqCst);
                        match next {
                            Cmd::Seek(t2) => t = t2,
                            other => {
                                pending_cmd = Some(other);
                                break;
                            }
                        }
                    }
                    do_seek(sh, &rd, ext.as_ref(), t, t0, &mut serial, &mut v_eof, &mut a_eof)
                }
                Cmd::SetAudio(id) => {
                    if let Some(e) = entries.iter().find(|e| e.id == id && e.playable).cloned() {
                        if out_rate > 0 {
                            let prev = sh.info.lock().unwrap().cur_audio.and_then(|c| entries.iter().find(|e| e.id == c).cloned());
                            let pos = sh.position();
                            if activate_audio(sh, &mut rd, &mut ext, &e, &mkv_src, out_rate, pos).is_ok() {
                                sh.info.lock().unwrap().cur_audio = Some(id);
                                do_seek(sh, &rd, ext.as_ref(), pos, t0, &mut serial, &mut v_eof, &mut a_eof);
                            } else if let Some(p) = prev {
                                let _ = activate_audio(sh, &mut rd, &mut ext, &p, &mkv_src, out_rate, pos);
                            }
                        }
                    }
                }
            }
            continue;
        }
        if idle {
            continue;
        }

        if last_stat.elapsed() > Duration::from_secs(5) {
            last_stat = Instant::now();
            let counts = (sh.underruns.load(Relaxed), sh.dropped.load(Relaxed));
            if counts != last_counts {
                last_counts = counts;
                logln!(
                    "stats: audio underruns={} late-dropped frames={} convert={:.1}ms audio-queue={:.2}s frames={} inflight={}",
                    counts.0,
                    counts.1,
                    sh.conv_us.load(Relaxed) as f64 / 1000.0,
                    sh.audio.lock().unwrap().q.len() as f64 / (2.0 * out_rate.max(1) as f64),
                    sh.frames.lock().unwrap().len(),
                    inflight.load(SeqCst)
                );
            }
        }

        // choose which stream to pull from, keeping both queues fed
        let a_room = !a_eof && sh.audio.lock().unwrap().q.len() < audio_cap;
        let v_room = !v_eof && inflight.load(SeqCst) + sh.frames.lock().unwrap().len() < VIDEO_Q;
        let stream = match (v_room, a_room) {
            (true, true) => ANY_STREAM,
            (true, false) => rd.vidx.unwrap_or(FIRST_VIDEO),
            (false, true) => rd.aidx.unwrap_or(ANY_STREAM),
            (false, false) => {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
        };

        let mut got_idx = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        let res = unsafe { rd.r.ReadSample(stream, 0, Some(&mut got_idx), Some(&mut flags), Some(&mut ts), Some(&mut sample)) };
        if let Err(e) = res {
            errors += 1;
            if errors > 50 {
                logln!("read error: {e}");
                v_eof = true;
                a_eof = true;
                sh.video_eof.store(true, SeqCst);
                sh.audio_eof.store(true, SeqCst);
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        errors = 0;
        if sh.serial.load(SeqCst) != serial {
            continue; // a seek was issued meanwhile
        }
        if flags & F_ERROR != 0 {
            v_eof = true;
            a_eof = true;
            sh.video_eof.store(true, SeqCst);
            sh.audio_eof.store(true, SeqCst);
            continue;
        }
        let is_video = Some(got_idx) == rd.vidx;
        if flags & F_TYPE_CHANGED != 0 && is_video {
            let (w, h) = video_size(&rd);
            vsize = (w, h);
            vstride = default_stride(&rd);
            let mut i = sh.info.lock().unwrap();
            i.width = w;
            i.height = h;
        }
        if flags & F_EOS != 0 {
            if is_video {
                v_eof = true;
                v_eos.store(true, SeqCst);
                if sample.is_none() && inflight.load(SeqCst) == 0 {
                    sh.video_eof.store(true, SeqCst);
                }
            } else if Some(got_idx) == rd.aidx {
                a_eof = true;
                sh.audio_eof.store(true, SeqCst);
            } else {
                v_eof = true;
                a_eof = true;
                sh.video_eof.store(true, SeqCst);
                sh.audio_eof.store(true, SeqCst);
            }
        }
        let Some(sample) = sample else { continue };
        let secs = ts as f64 / 1e7;
        if !t0_set {
            t0 = secs.max(0.0);
            t0_set = true;
            sh.t0.store(t0);
            sh.t0_ready.store(true, SeqCst);
        }
        let pts = secs - t0;

        if is_video {
            // precise seek: drop pictures before the requested position
            // (bounded: if decoding up to the target takes too long, show what we have instead of freezing)
            if pts < sh.seek_target.load() - 0.001 && seek_at.get().elapsed() < Duration::from_millis(350) {
                if v_eos.load(SeqCst) && inflight.load(SeqCst) == 0 {
                    sh.video_eof.store(true, SeqCst);
                }
                continue;
            }
            inflight.fetch_add(1, SeqCst);
            let job = VJob { sample: SendBox(sample), pts, serial, w: vsize.0, h: vsize.1, stride: vstride };
            if vtx.send(job).is_err() {
                inflight.fetch_sub(1, SeqCst);
            }
        } else if Some(got_idx) == rd.aidx {
            let decoded = if let Some(d) = rd.pure.borrow_mut().as_mut() {
                sample_to_bytes(&sample).map(|b| d.decode(&b))
            } else {
                sample_to_pcm(&sample)
            };
            if let Some(mut pcm) = decoded {
                if pcm.is_empty() {
                    continue;
                }
                let mut p = pts;
                let target = sh.seek_target.load();
                if p < target {
                    let skip = (((target - p) * out_rate as f64) as usize * 2).min(pcm.len());
                    pcm.drain(..skip);
                    p = target;
                    if pcm.is_empty() {
                        continue;
                    }
                }
                sh.push_audio(serial, p + sh.audio_delay.load(), &pcm);
            }
        }
    }
    Ok(())
}
