//! Software AC-3 / E-AC-3 (Dolby Digital / Digital Plus) decoding for streams Windows cannot decode
//! itself (stock Windows ships no Dolby decoder). Pure Rust (oxideav-ac3); output is stereo f32
//! resampled to the audio device rate.

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, TimeBase};

pub struct Dolby {
    dec: Box<dyn Decoder>,
    eac3: bool,
    in_rate: u32,
    out_rate: u32,
    // linear resampler state
    pos: f64,
    last: [f32; 2],
    have_last: bool,
    fails: u32,
}

impl Dolby {
    pub fn new(eac3: bool, in_rate: u32, out_rate: u32) -> Option<Dolby> {
        let mut p = CodecParameters::audio(CodecId::new(if eac3 { "eac3" } else { "ac3" }));
        p.channels = Some(2);
        p.sample_rate = Some(in_rate);
        let dec = if eac3 { oxideav_ac3::decoder::make_eac3_decoder(&p) } else { oxideav_ac3::decoder::make_decoder(&p) }.ok()?;
        Some(Dolby { dec, eac3, in_rate: in_rate.max(8000), out_rate: out_rate.max(8000), pos: 0.0, last: [0.0; 2], have_last: false, fails: 0 })
    }

    /// Forget all state (after a seek).
    pub fn reset(&mut self) {
        let _ = self.dec.reset();
        self.pos = 0.0;
        self.have_last = false;
    }

    /// Decode one container packet; returns interleaved stereo f32 at the output rate.
    pub fn decode(&mut self, data: &[u8]) -> Vec<f32> {
        let mut pcm: Vec<f32> = Vec::new();
        // a packet may hold several sync frames; split them so a bad one doesn't poison the rest
        for frame in split_frames(data, self.eac3) {
            let p = Packet::new(0, TimeBase::new(1, self.in_rate as i64), frame.to_vec());
            if let Err(e) = self.dec.send_packet(&p) {
                self.fails += 1;
                if self.fails <= 5 {
                    logln!("dolby decode error: {e:?}");
                }
                continue;
            }
            loop {
                match self.dec.receive_frame() {
                    Ok(Frame::Audio(f)) => {
                        if let Some(plane) = f.data.first() {
                            for c in plane.chunks_exact(2) {
                                pcm.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0);
                            }
                        }
                    }
                    Err(Error::NeedMore) | Err(Error::Eof) => break,
                    Err(_) => break,
                    Ok(_) => break,
                }
            }
        }
        if pcm.len() % 2 == 1 {
            pcm.pop();
        }
        self.resample(pcm)
    }

    fn resample(&mut self, input: Vec<f32>) -> Vec<f32> {
        if self.in_rate == self.out_rate || input.is_empty() {
            return input;
        }
        let step = self.in_rate as f64 / self.out_rate as f64;
        let n_in = input.len() / 2;
        let mut out = Vec::with_capacity((n_in as f64 / step) as usize * 2 + 4);
        // positions are indices into this block; index -1 is the previous block's last sample
        let get = |i: isize, last: [f32; 2], ch: usize| -> f32 {
            if i < 0 {
                last[ch]
            } else {
                input[(i as usize).min(n_in - 1) * 2 + ch]
            }
        };
        while self.pos < (n_in - 1) as f64 {
            let i0 = self.pos.floor() as isize; // may be -1 (the previous block's last sample)
            let fr = (self.pos - i0 as f64) as f32;
            for ch in 0..2 {
                let a = get(i0, self.last, ch);
                let b = get(i0 + 1, self.last, ch);
                out.push(a + (b - a) * fr);
            }
            self.pos += step;
        }
        self.pos -= n_in as f64;
        self.last = [input[(n_in - 1) * 2], input[(n_in - 1) * 2 + 1]];
        self.have_last = true;
        out
    }
}

/// Split a buffer into decodable units. AC-3: one sync frame each. E-AC-3: one access unit each, i.e.
/// an independent frame together with the dependent frames (7.1 extension) that follow it.
fn split_frames(data: &[u8], eac3: bool) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = Vec::new();
    let mut i = 0;
    let mut group_start: Option<usize> = None;
    while i + 8 <= data.len() {
        if data[i] != 0x0B || data[i + 1] != 0x77 {
            i += 1;
            continue;
        }
        let len = if eac3 {
            (((((data[i + 2] & 7) as usize) << 8) | data[i + 3] as usize) + 1) * 2
        } else {
            oxideav_ac3::syncinfo::parse(&data[i..]).map(|s| s.frame_length as usize).unwrap_or(0)
        };
        if len < 8 || i + len > data.len() {
            break;
        }
        if eac3 {
            let strmtyp = data[i + 2] >> 6;
            if strmtyp != 1 {
                if let Some(s) = group_start.take() {
                    v.push(&data[s..i]);
                }
                group_start = Some(i);
            } else if group_start.is_none() {
                group_start = Some(i);
            }
        } else {
            v.push(&data[i..i + len]);
        }
        i += len;
    }
    if let Some(s) = group_start {
        v.push(&data[s..i.min(data.len())]);
    }
    if v.is_empty() && !data.is_empty() {
        v.push(data);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_ac3_and_eac3_like_ffmpeg() {
        for (f, eac3) in [("/tmp/claude-0/c/t51.ac3", false), ("/tmp/claude-0/c/t51.eac3", true)] {
            let Ok(data) = std::fs::read(f) else { return };
            let mut d = Dolby::new(eac3, 48000, 44100).unwrap();
            let mut n = 0;
            let mut peak = 0f32;
            for fr in split_frames(&data, eac3) {
                let o = d.decode(fr);
                n += o.len() / 2;
                peak = o.iter().fold(peak, |a, b| a.max(b.abs()));
            }
            // 4 s of audio resampled to 44.1k
            assert!((n as i64 - 4 * 44100).abs() < 2000, "{f}: {n}");
            assert!(peak > 0.05, "{f}: silent");
        }
    }
}
