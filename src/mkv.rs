//! Minimal Matroska/WebM reader: track list (audio/subtitle names + languages) and *text subtitle*
//! extraction (SRT / ASS / SSA / WebVTT). Video/audio payloads are skipped with seeks, and clusters are
//! scanned by several threads in parallel, so even a multi-GB file loads its subtitles in a few seconds.

use crate::subs::{clean_text, Cue};
use crate::srcio::Src;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub number: u64,
    pub ttype: u64, // 1 video, 2 audio, 17 subtitle
    pub codec: String,
    pub lang: String,
    pub name: String,
    pub default: bool,
    pub forced: bool,
    pub channels: u32,
    pub sample_rate: f64,
    pub bit_depth: u32,
    pub private: Vec<u8>,
    pub default_duration_ns: u64,
    /// video: coded size in pixels
    pub width: u32,
    pub height: u32,
    /// bytes to put back in front of every frame (Matroska "header stripping" compression)
    pub strip_prefix: Vec<u8>,
}


impl Track {
    pub fn is_ass(&self) -> bool {
        self.codec.contains("ASS") || self.codec.contains("SSA")
    }
    /// Short, readable menu label: "Thai", "English (SDH)", "French - Canadian", "Track 5".
    pub fn label(&self) -> String {
        let lang = crate::lang::name(&self.lang);
        let name = self.name.trim();
        let mut l = if lang.is_empty() && name.is_empty() {
            format!("Track {}", self.number)
        } else if lang.is_empty() {
            name.to_string()
        } else if name.is_empty() {
            lang
        } else if name.to_lowercase().contains(&lang.to_lowercase()) {
            name.to_string()
        } else if name.len() <= 6 && name.chars().all(|c| c.is_ascii_uppercase()) {
            format!("{lang} ({name})")
        } else {
            format!("{lang} - {name}")
        };
        if self.forced {
            l.push_str(" (forced)");
        }
        l
    }
}

pub(crate) const SEGMENT: u32 = 0x1853_8067;
pub(crate) const INFO: u32 = 0x1549_A966;
pub(crate) const TRACKS: u32 = 0x1654_AE6B;
pub(crate) const TRACK_ENTRY: u32 = 0xAE;
pub(crate) const CLUSTER: u32 = 0x1F43_B675;
pub(crate) const BLOCK_GROUP: u32 = 0xA0;
pub(crate) const TIMECODE_SCALE: u32 = 0x2A_D7B1;
pub(crate) const TRACK_NUMBER: u32 = 0xD7;
pub(crate) const TRACK_TYPE: u32 = 0x83;
pub(crate) const CODEC_ID: u32 = 0x86;
pub(crate) const LANGUAGE: u32 = 0x22_B59C;
pub(crate) const LANGUAGE_BCP47: u32 = 0x22_B59D;
pub(crate) const NAME: u32 = 0x53_6E;
pub(crate) const FLAG_DEFAULT: u32 = 0x88;
pub(crate) const FLAG_FORCED: u32 = 0x55_AA;
pub(crate) const AUDIO: u32 = 0xE1;
pub(crate) const CHANNELS: u32 = 0x9F;
pub(crate) const CLUSTER_TC: u32 = 0xE7;
pub(crate) const SAMPLING_FREQ: u32 = 0xB5;
pub(crate) const BIT_DEPTH: u32 = 0x62_64;
pub(crate) const CODEC_PRIVATE: u32 = 0x63_A2;
pub(crate) const DEFAULT_DURATION: u32 = 0x23_E383;
pub(crate) const CONTENT_ENCODINGS: u32 = 0x6D_80;
pub(crate) const CONTENT_ENCODING: u32 = 0x62_40;
pub(crate) const CONTENT_COMPRESSION: u32 = 0x50_34;
pub(crate) const COMP_ALGO: u32 = 0x42_54;
pub(crate) const COMP_SETTINGS: u32 = 0x42_55;
pub(crate) const SIMPLE_BLOCK: u32 = 0xA3;
pub(crate) const BLOCK: u32 = 0xA1;
pub(crate) const BLOCK_DURATION: u32 = 0x9B;
pub(crate) const SEEKHEAD: u32 = 0x114D_9B74;
pub(crate) const SEEK: u32 = 0x4DBB;
pub(crate) const SEEK_ID: u32 = 0x53AB;
pub(crate) const SEEK_POS: u32 = 0x53AC;
pub(crate) const VIDEO: u32 = 0xE0;
pub(crate) const PIXEL_W: u32 = 0xB0;
pub(crate) const PIXEL_H: u32 = 0xBA;
pub(crate) const DURATION: u32 = 0x4489;
pub(crate) const CUES: u32 = 0x1C53_BB6B;
pub(crate) const CUE_POINT: u32 = 0xBB;
pub(crate) const CUE_TIME: u32 = 0xB3;
pub(crate) const CUE_TRACK_POS: u32 = 0xB7;
pub(crate) const CUE_CLUSTER_POS: u32 = 0xF1;

pub(crate) type R = BufReader<Src>;

pub(crate) fn open_r(path: &str, cap: usize) -> Option<R> {
    Some(BufReader::with_capacity(cap, Src::open(path)?))
}

/// (id, bytes used)
pub(crate) fn rd_id(r: &mut R) -> Option<(u32, u64)> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).ok()?;
    let len = b[0].leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut id = b[0] as u32;
    for _ in 1..len {
        r.read_exact(&mut b).ok()?;
        id = (id << 8) | b[0] as u32;
    }
    Some((id, len as u64))
}

/// (value, bytes used, unknown_size)
pub(crate) fn rd_vint(r: &mut R) -> Option<(u64, u64, bool)> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).ok()?;
    let len = b[0].leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let data_mask = (1u64 << (8 - len)) - 1;
    let mut v = (b[0] as u64) & data_mask;
    let mut all_ones = v == data_mask;
    for _ in 1..len {
        r.read_exact(&mut b).ok()?;
        if b[0] != 0xFF {
            all_ones = false;
        }
        v = (v << 8) | b[0] as u64;
    }
    Some((v, len as u64, all_ones))
}

pub(crate) fn rd_bytes(r: &mut R, n: u64) -> Option<Vec<u8>> {
    if n > 8 * 1024 * 1024 {
        r.seek_relative(n as i64).ok()?;
        return Some(Vec::new());
    }
    let mut v = vec![0u8; n as usize];
    r.read_exact(&mut v).ok()?;
    Some(v)
}

pub(crate) fn be_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, x| (a << 8) | *x as u64)
}

pub(crate) fn skip(r: &mut R, n: u64) -> Option<()> {
    r.seek_relative(n as i64).ok()
}

pub fn is_matroska(path: &str) -> bool {
    let mut b = [0u8; 4];
    Src::open(path).and_then(|mut f| f.read_exact(&mut b).ok()).is_some() && b == [0x1A, 0x45, 0xDF, 0xA3]
}

/// Size in bytes of a local file or link (None when unknown).
pub fn source_len(path: &str) -> Option<u64> {
    Src::open(path)?.len()
}

pub(crate) struct Head {
    pub(crate) tracks: Vec<Track>,
    pub(crate) scale: f64,
    /// file offset of the first Cluster element (None: no clusters)
    pub(crate) first_cluster: Option<u64>,
    /// file offset of the Cues element (from the SeekHead or found before the first cluster)
    pub(crate) cues_pos: Option<u64>,
    /// file offset where the Segment's data starts (Cues / SeekHead offsets are relative to it)
    pub(crate) segment_data: u64,
    /// Segment duration in seconds (0 = unknown)
    pub(crate) duration: f64,
}

/// Parse everything before the first cluster (track list, timecode scale).
pub(crate) fn read_head(r: &mut R) -> Option<Head> {
    let mut scale = 1_000_000f64;
    let mut tracks: Vec<Track> = Vec::new();
    let mut cur: Option<Track> = None;
    let mut in_audio = false;
    let mut first_cluster = None;
    let (mut seg_data, mut cues_pos, mut seek_id) = (0u64, None, 0u64);
    let mut duration_ticks = 0f64;
    loop {
        let pos = r.stream_position().ok()?;
        let Some((id, il)) = rd_id(r) else { break };
        let Some((size, sl, unknown)) = rd_vint(r) else { break };
        match id {
            SEEKHEAD | SEEK => {}
            SEEK_ID => seek_id = be_uint(&rd_bytes(r, size)?),
            SEEK_POS => {
                let v = be_uint(&rd_bytes(r, size)?);
                if seek_id == CUES as u64 {
                    cues_pos = Some(seg_data + v);
                }
            }
            CUES => {
                cues_pos = Some(pos);
                skip(r, size)?;
            }
            SEGMENT => seg_data = pos + il + sl,
            INFO | TRACKS | CONTENT_ENCODINGS | CONTENT_ENCODING | CONTENT_COMPRESSION => {}
            TRACK_ENTRY => {
                if let Some(t) = cur.take() {
                    tracks.push(t);
                }
                cur = Some(Track { default: true, ..Default::default() });
                in_audio = false;
            }
            AUDIO => in_audio = true,
            VIDEO => in_audio = false,
            DURATION => {
                let b = rd_bytes(r, size)?;
                duration_ticks = match b.len() {
                    4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
                    8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
                    _ => 0.0,
                };
            }
            CLUSTER => {
                first_cluster = Some(pos);
                break;
            }
            TIMECODE_SCALE => {
                let v = be_uint(&rd_bytes(r, size)?);
                if v > 0 {
                    scale = v as f64;
                }
            }
            SAMPLING_FREQ => {
                let b = rd_bytes(r, size)?;
                if let Some(t) = cur.as_mut() {
                    t.sample_rate = match b.len() {
                        4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
                        8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
                        _ => 0.0,
                    };
                }
            }
            CODEC_PRIVATE | COMP_SETTINGS => {
                let b = rd_bytes(r, size)?;
                if let Some(t) = cur.as_mut() {
                    if id == CODEC_PRIVATE {
                        t.private = b;
                    } else {
                        t.strip_prefix = b;
                    }
                }
            }
            TRACK_NUMBER | TRACK_TYPE | FLAG_DEFAULT | FLAG_FORCED | CHANNELS | BIT_DEPTH | DEFAULT_DURATION | COMP_ALGO | PIXEL_W | PIXEL_H => {
                let v = be_uint(&rd_bytes(r, size)?);
                if let Some(t) = cur.as_mut() {
                    match id {
                        BIT_DEPTH => t.bit_depth = v as u32,
                        PIXEL_W => t.width = v as u32,
                        PIXEL_H => t.height = v as u32,
                        DEFAULT_DURATION => t.default_duration_ns = v,
                        COMP_ALGO => {
                            if v != 3 {
                                t.strip_prefix.clear(); // only header stripping is understood
                            }
                        }
                        TRACK_NUMBER => t.number = v,
                        TRACK_TYPE => t.ttype = v,
                        FLAG_DEFAULT => t.default = v != 0,
                        FLAG_FORCED => t.forced = v != 0,
                        _ => {
                            if in_audio {
                                t.channels = v as u32
                            }
                        }
                    }
                }
            }
            CODEC_ID | LANGUAGE | LANGUAGE_BCP47 | NAME => {
                let b = rd_bytes(r, size)?;
                let s = String::from_utf8_lossy(&b).trim_end_matches('\0').to_string();
                if let Some(t) = cur.as_mut() {
                    match id {
                        CODEC_ID => t.codec = s,
                        LANGUAGE => {
                            if t.lang.is_empty() {
                                t.lang = s
                            }
                        }
                        LANGUAGE_BCP47 => t.lang = s,
                        _ => t.name = s,
                    }
                }
            }
            _ => {
                if unknown {
                    break;
                }
                skip(r, size)?;
            }
        }
    }
    if let Some(t) = cur.take() {
        tracks.push(t);
    }
    Some(Head { tracks, scale, first_cluster, cues_pos, segment_data: seg_data, duration: duration_ticks * scale / 1e9 })
}

/// All tracks of a local Matroska file (cheap: reads only the header).
pub fn read_tracks(path: &str) -> Option<Vec<Track>> {
    let mut r: R = open_r(path, 4096)?;
    read_head(&mut r).map(|h| h.tracks)
}

/// Tracks plus the Segment duration in seconds (0 = unknown).
pub fn read_tracks_duration(path: &str) -> Option<(Vec<Track>, f64)> {
    let mut r: R = open_r(path, 4096)?;
    read_head(&mut r).map(|h| (h.tracks, h.duration))
}

struct Pending {
    track: u64,
    start: f64,
    payload: Vec<u8>,
}

fn make_cue(tr: &Track, p: &Pending, dur: f64) -> Option<Cue> {
    let raw = String::from_utf8_lossy(&p.payload).into_owned();
    let text_raw = if tr.is_ass() {
        // "ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text"
        raw.splitn(9, ',').nth(8)?.to_string()
    } else {
        raw
    };
    let (text, top) = clean_text(&text_raw)?;
    Some(Cue { start: p.start, end: p.start + dur.max(0.05), text, top })
}

/// Cluster index for seeking and scanning: (timecode ticks, file offset of the Cluster element),
/// ordered by time, starting with the first cluster. Uses the Cues when the file has them (instant,
/// even for 26 GB remuxes and links); otherwise walks the cluster headers (one seek per cluster).
pub(crate) fn build_index(r: &mut R, head: &Head) -> Option<Vec<(u64, u64)>> {
    let first = head.first_cluster?;
    let from_cues = head.cues_pos.and_then(|cp| read_cues(r, cp, head.segment_data)).filter(|c| index_ok(r, c));
    let mut v = match from_cues {
        Some(v) => v,
        None => walk_index(r, first)?,
    };
    if v.first().map_or(true, |e| e.1 > first) {
        v.insert(0, (0, first));
    }
    Some(v)
}

/// Spot-check that cue positions really point at Cluster elements (a bad Cues would silence the audio).
fn index_ok(r: &mut R, c: &[(u64, u64)]) -> bool {
    if c.is_empty() {
        return false;
    }
    [0, c.len() / 2, c.len() - 1].iter().all(|&i| {
        r.seek(SeekFrom::Start(c[i].1)).is_ok() && matches!(rd_id(r), Some((CLUSTER, _)))
    })
}

fn walk_index(r: &mut R, first: u64) -> Option<Vec<(u64, u64)>> {
    let len = r.get_ref().len()?;
    r.seek(SeekFrom::Start(first)).ok()?;
    let mut v = Vec::new();
    let mut pos = first;
    while pos < len {
        let Some((id, il)) = rd_id(r) else { break };
        let Some((size, sl, unknown)) = rd_vint(r) else { break };
        if unknown {
            return None;
        }
        let data = pos + il + sl;
        if id == CLUSTER {
            // the cluster timecode is (almost always) the first child
            let tc = match (rd_id(r), rd_vint(r)) {
                (Some((CLUSTER_TC, _)), Some((n, _, _))) if n <= 8 => rd_bytes(r, n).map(|b| be_uint(&b)).unwrap_or(0),
                _ => v.last().map_or(0, |e: &(u64, u64)| e.0),
            };
            v.push((tc, pos));
        }
        pos = data + size;
        r.seek(SeekFrom::Start(pos)).ok()?;
    }
    if v.is_empty() { None } else { Some(v) }
}

/// Scan everything in `from..to`: top-level clusters one after the other.
fn scan_range(r: &mut R, from: u64, to: u64, tracks: &[Track], scale: f64, quit: &AtomicBool, out: &mut Vec<(u64, Cue)>) -> Option<()> {
    let mut pos = from;
    while pos < to && !quit.load(Ordering::Relaxed) {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let (id, il) = rd_id(r)?;
        let (size, sl, unknown) = rd_vint(r)?;
        if unknown {
            return None;
        }
        let data = pos + il + sl;
        if id == CLUSTER {
            let _ = scan_region(r, data, size, tracks, scale, quit, out);
        }
        pos = data + size;
    }
    Some(())
}

/// Scan the body of one cluster (or, with size == u64::MAX, everything from `off` to EOF).
fn scan_region(r: &mut R, off: u64, size: u64, tracks: &[Track], scale: f64, quit: &AtomicBool, out: &mut Vec<(u64, Cue)>) -> Option<()> {
    r.seek(SeekFrom::Start(off)).ok()?;
    let mut consumed = 0u64;
    let mut cluster_tc = 0f64;
    let mut pending: Option<Pending> = None;
    let flush = |p: Pending, dur: f64, out: &mut Vec<(u64, Cue)>| {
        if let Some(tr) = tracks.iter().find(|t| t.number == p.track) {
            if let Some(c) = make_cue(tr, &p, dur) {
                out.push((p.track, c));
            }
        }
    };
    while consumed < size && !quit.load(Ordering::Relaxed) {
        let Some((id, il)) = rd_id(r) else { break };
        let Some((sz, sl, unknown)) = rd_vint(r) else { break };
        consumed += il + sl;
        if let Some(p) = pending.take() {
            if matches!(id, BLOCK_GROUP | SIMPLE_BLOCK | CLUSTER | BLOCK) {
                flush(p, 4.0, out);
            } else {
                pending = Some(p);
            }
        }
        match id {
            BLOCK_GROUP | CLUSTER => {} // descend
            CLUSTER_TC => {
                cluster_tc = be_uint(&rd_bytes(r, sz)?) as f64;
                consumed += sz;
            }
            SIMPLE_BLOCK | BLOCK => {
                if unknown {
                    break;
                }
                let (tn, tl, _) = rd_vint(r)?;
                let mut h = [0u8; 3];
                r.read_exact(&mut h).ok()?;
                let payload_len = sz.saturating_sub(tl + 3);
                let is_sub = tracks.iter().any(|t| t.number == tn);
                let laced = h[2] & 0x06 != 0;
                if is_sub && !laced && payload_len < 1_000_000 {
                    let payload = rd_bytes(r, payload_len)?;
                    let rel = i16::from_be_bytes([h[0], h[1]]) as f64;
                    let start = (cluster_tc + rel) * scale / 1e9;
                    let p = Pending { track: tn, start, payload };
                    if id == SIMPLE_BLOCK {
                        flush(p, 4.0, out);
                    } else {
                        pending = Some(p);
                    }
                } else {
                    skip(r, payload_len)?;
                }
                consumed += sz;
            }
            BLOCK_DURATION => {
                let v = be_uint(&rd_bytes(r, sz)?) as f64 * scale / 1e9;
                consumed += sz;
                if let Some(p) = pending.take() {
                    flush(p, v, out);
                }
            }
            _ => {
                if unknown {
                    break;
                }
                skip(r, sz)?;
                consumed += sz;
            }
        }
    }
    if let Some(p) = pending.take() {
        flush(p, 4.0, out);
    }
    Some(())
}

/// Which region of the file to read next: nearest to the playback position first (forward 3 : back 1),
/// so the subtitles around the current position appear within a moment, even in a 26 GB file.
struct Picker {
    claimed: Vec<bool>,
    center: usize,
    fwd: usize,
    back: isize,
    n: u32,
}

impl Picker {
    fn pick(&mut self, c: usize) -> Option<usize> {
        if self.center != c || self.n == 0 {
            self.center = c;
            self.fwd = c;
            self.back = c as isize - 1;
        }
        self.n = self.n.wrapping_add(1);
        let prefer_back = self.n % 4 == 0;
        for step in 0..2 {
            let back = prefer_back == (step == 0);
            if back {
                while self.back >= 0 && self.claimed[self.back as usize] {
                    self.back -= 1;
                }
                if self.back >= 0 {
                    let i = self.back as usize;
                    self.claimed[i] = true;
                    return Some(i);
                }
            } else {
                while self.fwd < self.claimed.len() && self.claimed[self.fwd] {
                    self.fwd += 1;
                }
                if self.fwd < self.claimed.len() {
                    let i = self.fwd;
                    self.claimed[i] = true;
                    return Some(i);
                }
            }
        }
        None
    }
}

/// Scan `path`. `focus` gives the playback position (seconds) the scan should start around.
/// `on_tracks` is called once with the text-subtitle tracks found; `on_cues` receives batches
/// `(track_number, cue)` from several threads. Returns when everything has been read.
pub fn scan(path: &str, quit: &AtomicBool, focus: impl Fn() -> f64 + Sync, on_tracks: impl FnOnce(&[Track]), on_cues: impl Fn(Vec<(u64, Cue)>) + Sync) -> Option<()> {
    let mut r: R = open_r(path, 512)?;
    let head = read_head(&mut r)?;
    let first = head.first_cluster?;
    let index = build_index(&mut r, &head);
    let len = r.get_ref().len().unwrap_or(u64::MAX);
    let scale = head.scale;
    let subs: Vec<Track> = head
        .tracks
        .into_iter()
        .filter(|t| t.ttype == 17 && (t.codec.starts_with("S_TEXT/") || t.codec == "S_ASS" || t.codec == "S_SSA"))
        .collect();
    if subs.is_empty() {
        return Some(());
    }
    on_tracks(&subs);
    let subs_ref = &subs;
    match index {
        Some(mut ix) => {
            ix.sort_by_key(|e| e.1);
            // region i = byte range from cluster i to cluster i+1
            let regions: Vec<(f64, u64, u64)> = ix.iter().enumerate().map(|(i, &(tc, off))| (tc as f64 * scale / 1e9, off, ix.get(i + 1).map_or(len, |n| n.1))).collect();
            let picker = std::sync::Mutex::new(Picker { claimed: vec![false; regions.len()], center: usize::MAX, fwd: 0, back: -1, n: 0 });
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
            let (regions, picker, on_cues, focus) = (&regions, &picker, &on_cues, &focus);
            std::thread::scope(|s| {
                for _ in 0..n {
                    s.spawn(move || {
                        let Some(mut r) = open_r(path, 512) else { return };
                        while !quit.load(Ordering::Relaxed) {
                            let f = focus();
                            let c = regions.partition_point(|x| x.0 <= f).saturating_sub(1);
                            let Some(i) = picker.lock().unwrap().pick(c) else { break };
                            let (_, from, to) = regions[i];
                            let mut out = Vec::new();
                            let _ = scan_range(&mut r, from, to, subs_ref, scale, quit, &mut out);
                            if !out.is_empty() {
                                on_cues(out);
                            }
                        }
                    });
                }
            });
        }
        None => {
            // live/unknown-size layout: one linear pass
            let mut out = Vec::new();
            let mut r: R = open_r(path, 512)?;
            r.seek(SeekFrom::Start(first)).ok()?;
            let _ = scan_region(&mut r, first, u64::MAX, subs_ref, scale, quit, &mut out);
            on_cues(out);
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scans_generated_mkv() {
        let p = "/tmp/vt/t.mkv";
        if !std::path::Path::new(p).exists() {
            return;
        }
        let quit = AtomicBool::new(false);
        let mut tr = vec![];
        let cues = std::sync::Mutex::new(vec![]);
        scan(p, &quit, || 0.0, |t| tr = t.to_vec(), |v| cues.lock().unwrap().extend(v));
        let mut cues = cues.into_inner().unwrap();
        cues.sort_by(|a, b| a.1.start.partial_cmp(&b.1.start).unwrap());
        assert_eq!(tr.len(), 1, "{tr:?}");
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert!(cues[0].1.text.contains("สวัสดี"));
        assert!((cues[0].1.start - 1.0).abs() < 0.01 && (cues[0].1.end - 3.0).abs() < 0.01);
        assert!(cues[1].1.top);
    }

    #[test]
    fn scans_ass_track() {
        let p = "/tmp/vt/a.mkv";
        if !std::path::Path::new(p).exists() {
            return;
        }
        let quit = AtomicBool::new(false);
        let mut tr = vec![];
        let cues = std::sync::Mutex::new(vec![]);
        scan(p, &quit, || 0.0, |t| tr = t.to_vec(), |v| cues.lock().unwrap().extend(v));
        let mut cues = cues.into_inner().unwrap();
        cues.sort_by(|a, b| a.1.start.partial_cmp(&b.1.start).unwrap());
        assert!(tr[0].is_ass(), "{tr:?}");
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert!(cues[0].1.text.contains("ทดสอบซับ") && !cues[0].1.text.contains('{'));
    }
}

#[cfg(test)]
mod tests2 {
    use super::*;
    #[test]
    fn parallel_scan_and_labels() {
        let p = "/tmp/claude-0/m/big.mkv";
        if !std::path::Path::new(p).exists() {
            return;
        }
        let tr = read_tracks(p).unwrap();
        let audio: Vec<_> = tr.iter().filter(|t| t.ttype == 2).collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].lang, "eng");
        assert_eq!(audio[0].channels, 1);
        assert_eq!(crate::lang::name(&audio[1].lang), "Japanese");
        let quit = AtomicBool::new(false);
        let mut labels = vec![];
        let cues = std::sync::Mutex::new(Vec::new());
        scan(p, &quit, || 0.0, |t| labels = t.iter().map(|x| x.label()).collect(), |v| cues.lock().unwrap().extend(v));
        assert_eq!(labels, vec!["Thai", "English (SDH)", "French - Canadian"]);
        let cues = cues.into_inner().unwrap();
        assert_eq!(cues.len(), 900, "{}", cues.len());
    }
}

/// Cluster start offsets and times (timecode ticks) from the Cues element, sorted by time.
/// (Used for links, where walking every cluster header would need one request each.)
pub(crate) fn read_cues(r: &mut R, cues_pos: u64, segment_data: u64) -> Option<Vec<(u64, u64)>> {
    r.seek(SeekFrom::Start(cues_pos)).ok()?;
    let (id, _) = rd_id(r)?;
    if id != CUES {
        return None;
    }
    let (size, _, unknown) = rd_vint(r)?;
    if unknown || size > 64 * 1024 * 1024 {
        return None;
    }
    let data = rd_bytes(r, size)?;
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut cur = std::io::Cursor::new(&data[..]);
    let mut time = 0u64;
    let end = data.len() as u64;
    // flat walk: CuePoint and CueTrackPositions are containers, everything else is a leaf
    while cur.position() < end {
        let (Some((id, _)), Some((sz, _, _))) = (rd_id_c(&mut cur), rd_vint_c(&mut cur)) else { break };
        match id {
            CUE_POINT | CUE_TRACK_POS => {}
            _ => {
                let at = cur.position() as usize;
                let b = data.get(at..at + sz as usize)?;
                match id {
                    CUE_TIME => time = be_uint(b),
                    CUE_CLUSTER_POS => out.push((time, segment_data + be_uint(b))),
                    _ => {}
                }
                cur.set_position(at as u64 + sz);
            }
        }
    }
    out.sort();
    out.dedup_by_key(|c| c.1);
    if out.is_empty() { None } else { Some(out) }
}

fn rd_id_c(c: &mut std::io::Cursor<&[u8]>) -> Option<(u32, u64)> {
    let mut b = [0u8; 1];
    c.read_exact(&mut b).ok()?;
    let len = b[0].leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut id = b[0] as u32;
    for _ in 1..len {
        c.read_exact(&mut b).ok()?;
        id = (id << 8) | b[0] as u32;
    }
    Some((id, len as u64))
}

fn rd_vint_c(c: &mut std::io::Cursor<&[u8]>) -> Option<(u64, u64, bool)> {
    let mut b = [0u8; 1];
    c.read_exact(&mut b).ok()?;
    let len = b[0].leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mask = (1u64 << (8 - len)) - 1;
    let mut v = (b[0] as u64) & mask;
    for _ in 1..len {
        c.read_exact(&mut b).ok()?;
        v = (v << 8) | b[0] as u64;
    }
    Some((v, len as u64, false))
}

#[cfg(test)]
mod tests3 {
    use super::*;

    #[test]
    fn cue_index_matches_walk() {
        for p in ["/tmp/claude-0/m/big.mkv", "/tmp/claude-0/a/thd.mkv", "/tmp/claude-0/v/av1.mkv"] {
            if !std::path::Path::new(p).exists() {
                continue;
            }
            let mut r: R = open_r(p, 4096).unwrap();
            let head = read_head(&mut r).unwrap();
            let by_cues = build_index(&mut r, &head).unwrap();
            let walked = walk_index(&mut r, head.first_cluster.unwrap()).unwrap();
            // every cue entry is a real cluster, in the walked list
            for e in &by_cues {
                assert!(walked.iter().any(|w| w.1 == e.1), "{p}: cue {e:?} is not a cluster");
            }
            assert_eq!(by_cues[0].1, walked[0].1, "{p}");
            eprintln!("{p}: {} cue entries, {} clusters", by_cues.len(), walked.len());
        }
    }

    #[test]
    fn picker_starts_at_focus_and_covers_all() {
        let mut p = Picker { claimed: vec![false; 20], center: usize::MAX, fwd: 0, back: -1, n: 0 };
        let first: Vec<usize> = (0..4).map(|_| p.pick(10).unwrap()).collect();
        assert_eq!(first, vec![10, 11, 12, 9]);
        // a seek elsewhere re-centres
        assert_eq!(p.pick(2), Some(2));
        let mut seen = std::collections::HashSet::new();
        while let Some(i) = p.pick(5) {
            assert!(seen.insert(i));
        }
        assert_eq!(seen.len() + 5, 20);
    }
}
