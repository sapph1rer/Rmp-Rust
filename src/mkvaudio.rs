//! Our own Matroska audio demuxer: pulls the packets of one audio track straight from the file, so
//! codecs Windows cannot decode (DTS, TrueHD, Opus, Vorbis, E-AC-3 ...) can be handled by the
//! pure-Rust decoders in `codecs.rs`. Seeks use the cluster timecodes (no Cues needed).

use crate::mkv::*;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

pub struct Demux {
    r: R,
    /// (timecode ticks, file offset of the Cluster element): seek points only
    index: Vec<(u64, u64)>,
    scale: f64, // ns per timecode tick
    track: u64,
    prefix: Vec<u8>,
    default_dur: f64,
    /// where the next top-level element starts; clusters are read one after the other
    pos: u64,
    len: u64,
    queue: VecDeque<(f64, Vec<u8>)>,
}

impl Demux {
    /// Opening is cheap when the file has Cues (a 26 GB remux opens at once); without them every
    /// cluster header is visited once, so call this from a worker thread, not the UI/reader thread.
    pub fn open(path: &str, track_number: u64) -> Option<Demux> {
        let mut r: R = open_r(path, 4096)?;
        let head = read_head(&mut r)?;
        let track = head.tracks.iter().find(|t| t.number == track_number)?.clone();
        let first = head.first_cluster?;
        let index = build_index(&mut r, &head)?;
        let len = r.get_ref().len().unwrap_or(u64::MAX);
        Some(Demux {
            r,
            index,
            scale: head.scale,
            track: track_number,
            prefix: track.strip_prefix.clone(),
            default_dur: track.default_duration_ns as f64 / 1e9,
            pos: first,
            len,
            queue: VecDeque::new(),
        })
    }

    /// Position so that the next packet comes from the cluster containing `t` (seconds).
    pub fn seek(&mut self, t: f64) {
        self.queue.clear();
        let ticks = (t.max(0.0) * 1e9 / self.scale) as u64;
        let i = self.index.partition_point(|c| c.0 <= ticks);
        self.pos = self.index[i.saturating_sub(1)].1;
    }

    pub fn next_packet(&mut self) -> Option<(f64, Vec<u8>)> {
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Some(p);
            }
            if self.pos >= self.len {
                return None;
            }
            if self.r.seek(SeekFrom::Start(self.pos)).is_err() {
                return None;
            }
            let (id, il) = rd_id(&mut self.r)?;
            let (sz, sl, unknown) = rd_vint(&mut self.r)?;
            if unknown {
                return None; // live layouts are not supported
            }
            let data = self.pos + il + sl;
            self.pos = data + sz;
            if id == CLUSTER {
                // a damaged cluster is dropped, the next one is read
                let _ = self.read_cluster(data, sz);
            }
        }
    }

    fn read_cluster(&mut self, data: u64, size: u64) -> Option<()> {
        self.r.seek(SeekFrom::Start(data)).ok()?;
        let mut consumed = 0u64;
        let mut cluster_tc = 0f64;
        while consumed < size {
            let (id, il) = rd_id(&mut self.r)?;
            let (sz, sl, unknown) = rd_vint(&mut self.r)?;
            consumed += il + sl;
            match id {
                BLOCK_GROUP => {}
                CLUSTER_TC => {
                    cluster_tc = be_uint(&rd_bytes(&mut self.r, sz)?) as f64;
                    consumed += sz;
                }
                SIMPLE_BLOCK | BLOCK => {
                    if unknown {
                        return None;
                    }
                    let (tn, tl, _) = rd_vint(&mut self.r)?;
                    let mut h = [0u8; 3];
                    self.r.read_exact(&mut h).ok()?;
                    let payload_len = sz.saturating_sub(tl + 3);
                    if tn == self.track && payload_len < 64 * 1024 * 1024 {
                        let mut data = vec![0u8; payload_len as usize];
                        self.r.read_exact(&mut data).ok()?;
                        let rel = i16::from_be_bytes([h[0], h[1]]) as f64;
                        let pts = (cluster_tc + rel) * self.scale / 1e9;
                        self.push_block(pts, h[2], &data);
                    } else {
                        skip(&mut self.r, payload_len)?;
                    }
                    consumed += sz;
                }
                _ => {
                    if unknown {
                        return None;
                    }
                    skip(&mut self.r, sz)?;
                    consumed += sz;
                }
            }
        }
        Some(())
    }

    fn push_block(&mut self, pts: f64, flags: u8, data: &[u8]) {
        let frames = split_lacing((flags >> 1) & 3, data);
        for (i, f) in frames.into_iter().enumerate() {
            let t = pts + i as f64 * self.default_dur;
            if self.prefix.is_empty() {
                self.queue.push_back((t, f.to_vec()));
            } else {
                let mut v = self.prefix.clone();
                v.extend_from_slice(f);
                self.queue.push_back((t, v));
            }
        }
    }
}

fn vint_at(d: &[u8], pos: &mut usize) -> Option<u64> {
    let b = *d.get(*pos)?;
    let len = b.leading_zeros() as usize + 1;
    if len > 8 || *pos + len > d.len() {
        return None;
    }
    let mut v = (b as u64) & ((1u64 << (8 - len)) - 1);
    for k in 1..len {
        v = (v << 8) | d[*pos + k] as u64;
    }
    *pos += len;
    Some(v)
}

/// Split a block payload into its frames (0 = no lacing, 1 = Xiph, 2 = fixed, 3 = EBML).
pub fn split_lacing(mode: u8, d: &[u8]) -> Vec<&[u8]> {
    if mode == 0 || d.is_empty() {
        return vec![d];
    }
    let n = d[0] as usize + 1;
    let mut pos = 1usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(n);
    match mode {
        1 => {
            for _ in 0..n - 1 {
                let mut s = 0usize;
                loop {
                    let Some(&b) = d.get(pos) else { return vec![] };
                    pos += 1;
                    s += b as usize;
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(s);
            }
        }
        3 => {
            let Some(first) = vint_at(d, &mut pos) else { return vec![] };
            let mut cur = first as i64;
            sizes.push(cur as usize);
            for _ in 1..n - 1 {
                let start = pos;
                let Some(raw) = vint_at(d, &mut pos) else { return vec![] };
                let len = pos - start;
                let bias = (1i64 << (7 * len - 1)) - 1;
                cur += raw as i64 - bias;
                if cur < 0 {
                    return vec![];
                }
                sizes.push(cur as usize);
            }
        }
        _ => {
            let each = (d.len() - pos) / n;
            sizes.extend(std::iter::repeat(each).take(n - 1));
        }
    }
    let used: usize = sizes.iter().sum();
    if pos + used > d.len() {
        return vec![];
    }
    sizes.push(d.len() - pos - used);
    let mut out = Vec::with_capacity(n);
    for s in sizes {
        out.push(&d[pos..pos + s]);
        pos += s;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lacing() {
        // Xiph: 3 frames of 2,3,4 bytes
        let d = [2u8, 2, 3, 1, 1, 2, 2, 2, 3, 3, 3, 3];
        let f = split_lacing(1, &d);
        assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![2, 3, 4]);
        // fixed: 2 frames of 3
        let d = [1u8, 1, 1, 1, 2, 2, 2];
        assert_eq!(split_lacing(2, &d).len(), 2);
        // EBML: first size 3 (0x83), diff +1 (0xC0 = bias 63 -> 0xBF+... use raw 64 -> 0xC0)
        let d = [2u8, 0x83, 0xC0, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3];
        let f = split_lacing(3, &d);
        assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![3, 4, 5]);
    }
}
