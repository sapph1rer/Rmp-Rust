//! Subtitle store + parsers (SRT / WebVTT / ASS-SSA files, embedded text streams).
//! Everything is turned into plain `Cue`s (text + time); styling is intentionally
//! not kept, the UI draws them with one outlined font.

use anyhow::{bail, Result};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub top: bool,
}

pub struct SubTrack {
    pub id: usize,
    pub label: String,
    pub cues: Vec<Cue>,
    pub max_dur: f64,
    /// menu order (lower first)
    pub pri: u8,
}

#[derive(Default)]
pub struct SubStore {
    /// embedded subtitles are still being read from the file
    pub loading: bool,
    pub tracks: Vec<SubTrack>,
    pub active: Option<usize>,
    pub delay: f64,
    pub next_ext: usize,
}

impl SubStore {
    pub fn add_track(&mut self, id: usize, label: String) {
        if !self.tracks.iter().any(|t| t.id == id) {
            self.tracks.push(SubTrack { id, label, cues: vec![], max_dur: 0.0, pri: 5 });
        }
    }

    pub fn set_priority(&mut self, id: usize, pri: u8) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
            t.pri = pri;
        }
    }

    /// Insert keeping the list sorted by start time; ignores exact duplicates.
    pub fn push_cue(&mut self, id: usize, cue: Cue) {
        let Some(tr) = self.tracks.iter_mut().find(|t| t.id == id) else { return };
        let at = tr.cues.partition_point(|c| c.start <= cue.start);
        let lo = at.saturating_sub(6);
        if tr.cues[lo..at].iter().any(|c| c.start == cue.start && c.text == cue.text) {
            return;
        }
        tr.max_dur = tr.max_dur.max(cue.end - cue.start);
        tr.cues.insert(at, cue);
    }

    pub fn set_cues(&mut self, id: usize, mut cues: Vec<Cue>) {
        let Some(tr) = self.tracks.iter_mut().find(|t| t.id == id) else { return };
        cues.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
        tr.max_dur = cues.iter().map(|c| c.end - c.start).fold(0.0, f64::max);
        tr.cues = cues;
    }

    /// Cues visible at media time `t` for the active track.
    pub fn active_cues(&self, t: f64) -> Vec<&Cue> {
        let t = t - self.delay;
        let Some(tr) = self.tracks.iter().find(|x| Some(x.id) == self.active) else { return vec![] };
        let mut i = tr.cues.partition_point(|c| c.start <= t);
        let mut out = Vec::new();
        while i > 0 && out.len() < 6 {
            i -= 1;
            let c = &tr.cues[i];
            if c.start < t - tr.max_dur {
                break;
            }
            if c.end > t {
                out.push(c);
            }
        }
        out.reverse();
        out
    }

    pub fn cycle(&mut self) {
        let ids: Vec<usize> = self.tracks.iter().map(|t| t.id).collect();
        self.active = match self.active {
            None => ids.first().copied(),
            Some(cur) => {
                let p = ids.iter().position(|&i| i == cur).unwrap_or(0);
                ids.get(p + 1).copied() // past the last one -> off
            }
        };
    }
}

// ---------------------------------------------------------------- text helpers

/// Decode a subtitle file: BOM / UTF-16 / UTF-8, otherwise Windows-874 (Thai) / Latin-1.
pub fn decode_text(b: &[u8]) -> String {
    if let Some(r) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(r).into_owned();
    }
    if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        let le = b[0] == 0xFF;
        let u: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return String::from_utf16_lossy(&u);
    }
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    b.iter()
        .map(|&x| match x {
            0..=0x7F => x as char,
            0xA1..=0xDA => char::from_u32(0x0E01 + (x as u32 - 0xA1)).unwrap_or('?'),
            0xDF..=0xFB => char::from_u32(0x0E3F + (x as u32 - 0xDF)).unwrap_or('?'),
            0x80 => '€',
            0x85 => '…',
            0x91 => '‘',
            0x92 => '’',
            0x93 => '“',
            0x94 => '”',
            0x96 => '–',
            0x97 => '—',
            _ => ' ',
        })
        .collect()
}

/// Strip ASS override tags / HTML tags. Returns (text, top-aligned). None = drawing / empty.
pub fn clean_text(raw: &str) -> Option<(String, bool)> {
    let mut out = String::with_capacity(raw.len());
    let mut top = false;
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' => {
                let mut tag = String::new();
                for d in it.by_ref() {
                    if d == '}' {
                        break;
                    }
                    tag.push(d);
                }
                if tag.contains("\\an7") || tag.contains("\\an8") || tag.contains("\\an9") {
                    top = true;
                }
                if let Some(p) = tag.find("\\p") {
                    // \p1 .. = vector drawing (signs); \p0 turns it off
                    if tag[p + 2..].chars().next().map_or(false, |d| ('1'..='9').contains(&d)) {
                        return None;
                    }
                }
            }
            '<' => {
                // html-ish tag in srt/vtt: <i> </b> <font ...> <c.x> <00:00:01.000>
                let nxt = it.peek().copied().unwrap_or(' ');
                if nxt.is_ascii_alphanumeric() || nxt == '/' {
                    for d in it.by_ref() {
                        if d == '>' {
                            break;
                        }
                    }
                } else {
                    out.push('<');
                }
            }
            '\\' => match it.peek().copied() {
                Some('N') | Some('n') => {
                    it.next();
                    out.push('\n');
                }
                Some('h') => {
                    it.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    let t = out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ");
    let t = t.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    if t.is_empty() {
        None
    } else {
        Some((t, top))
    }
}

fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let mut secs = 0.0;
    for p in &parts {
        secs = secs * 60.0 + p.trim().parse::<f64>().ok()?;
    }
    Some(secs)
}

fn parse_srt_vtt(text: &str) -> Vec<Cue> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(ti) = lines.iter().position(|l| l.contains("-->")) else { continue };
        let mut sp = lines[ti].split("-->");
        let (Some(a), Some(b)) = (sp.next(), sp.next()) else { continue };
        let b = b.trim().split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_time(a), parse_time(b)) else { continue };
        if let Some((text, top)) = clean_text(&lines[ti + 1..].join("\n")) {
            cues.push(Cue { start, end, text, top });
        }
    }
    cues
}

fn parse_ass(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut in_events = false;
    let (mut n, mut is, mut ie, mut it) = (10usize, 1usize, 2usize, 9usize);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_events = l.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = l.strip_prefix("Format:") {
            let names: Vec<String> = f.split(',').map(|x| x.trim().to_lowercase()).collect();
            n = names.len();
            is = names.iter().position(|x| x == "start").unwrap_or(1);
            ie = names.iter().position(|x| x == "end").unwrap_or(2);
            it = names.iter().position(|x| x == "text").unwrap_or(n - 1);
        } else if let Some(d) = l.strip_prefix("Dialogue:") {
            let f: Vec<&str> = d.trim_start().splitn(n, ',').collect();
            if f.len() < n {
                continue;
            }
            let (Some(start), Some(end)) = (parse_time(f[is]), parse_time(f[ie])) else { continue };
            if let Some((text, top)) = clean_text(f[it]) {
                cues.push(Cue { start, end, text, top });
            }
        }
    }
    cues
}

/// Load an external subtitle file (.srt .vtt .ass .ssa).
pub fn load_file(path: &str) -> Result<Vec<Cue>> {
    let bytes = std::fs::read(path)?;
    let text = decode_text(&bytes);
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let cues = if ext == "ass" || ext == "ssa" || text.contains("[Events]") {
        parse_ass(&text)
    } else {
        parse_srt_vtt(&text)
    };
    if cues.is_empty() {
        bail!("no subtitle cues found in {path}");
    }
    Ok(cues)
}

pub fn is_sub_ext(path: &str) -> bool {
    let e = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(e.as_str(), "srt" | "ass" | "ssa" | "vtt" | "sub")
}

