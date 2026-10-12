//! Pure-Rust audio decoders for formats Windows Media Foundation cannot (or may not) decode:
//! AC-3, E-AC-3, DTS (core), Dolby TrueHD, Opus and Vorbis. Every decoder is wrapped so the caller
//! always gets interleaved stereo f32 at the audio device's sample rate.

use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, TimeBase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ac3,
    Eac3,
    Dts,
    TrueHd,
    Opus,
    Vorbis,
}

impl Kind {
    /// Matroska CodecID -> decoder
    pub fn from_mkv(codec_id: &str) -> Option<Kind> {
        match codec_id {
            "A_AC3" => Some(Kind::Ac3),
            "A_EAC3" => Some(Kind::Eac3),
            "A_TRUEHD" => Some(Kind::TrueHd),
            "A_OPUS" => Some(Kind::Opus),
            "A_VORBIS" => Some(Kind::Vorbis),
            c if c.starts_with("A_DTS") => Some(Kind::Dts),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Ac3 => "AC3",
            Kind::Eac3 => "E-AC3",
            Kind::Dts => "DTS",
            Kind::TrueHd => "TrueHD",
            Kind::Opus => "Opus",
            Kind::Vorbis => "Vorbis",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    L,
    R,
    C,
    Ls,
    Rs,
    S,
    Bl,
    Br,
    Lfe,
    Skip,
}

/// Channel layout of Vorbis / Opus (family 1) streams by channel count.
fn vorbis_roles(n: usize) -> Vec<Role> {
    use Role::*;
    match n {
        1 => vec![C],
        2 => vec![L, R],
        3 => vec![L, C, R],
        4 => vec![L, R, Ls, Rs],
        5 => vec![L, C, R, Ls, Rs],
        6 => vec![L, C, R, Ls, Rs, Lfe],
        7 => vec![L, C, R, Ls, Rs, S, Lfe],
        8 => vec![L, C, R, Ls, Rs, Bl, Br, Lfe],
        _ => {
            let mut v = vec![L, R];
            v.resize(n, Skip);
            v
        }
    }
}

static MIX: [std::sync::atomic::AtomicU32; 3] = [
    std::sync::atomic::AtomicU32::new(0x3F80_0000), // centre 1.0
    std::sync::atomic::AtomicU32::new(0x3F80_0000), // surround 1.0
    std::sync::atomic::AtomicU32::new(0),           // LFE 0.0
];

/// Stereo downmix levels (settings): dialogue (centre), surround and LFE gain; applies to all codecs.
pub fn set_mix(center: f32, surround: f32, lfe: f32) {
    for (m, v) in MIX.iter().zip([center, surround, lfe]) {
        m.store(v.clamp(0.0, 4.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
}

fn mix_levels() -> (f32, f32, f32) {
    let g = |i: usize| f32::from_bits(MIX[i].load(std::sync::atomic::Ordering::Relaxed));
    (g(0), g(1), g(2))
}

/// ITU-style downmix of planar channels to interleaved stereo.
fn mixdown(planes: &[Vec<f32>], roles: &[Role], out: &mut Vec<f32>) {
    let n = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    if n == 0 {
        return;
    }
    const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let only_lr = roles.iter().all(|r| matches!(r, Role::L | Role::R | Role::Lfe | Role::Skip));
    let only_c = roles.iter().filter(|r| !matches!(r, Role::Lfe | Role::Skip)).count() == 1 && roles.contains(&Role::C);
    let scale = if only_lr || only_c { 1.0 } else { 1.0 / (1.0 + K) };
    let (gc, gs, glfe) = mix_levels();
    out.reserve(n * 2);
    for i in 0..n {
        let (mut l, mut r) = (0f32, 0f32);
        for (p, role) in planes.iter().zip(roles.iter()) {
            let x = p[i];
            match role {
                Role::L => l += x,
                Role::R => r += x,
                Role::C => {
                    if only_c {
                        l += x;
                        r += x;
                    } else {
                        l += x * K * gc;
                        r += x * K * gc;
                    }
                }
                Role::Ls => l += x * K * gs,
                Role::Rs => r += x * K * gs,
                Role::S => {
                    l += x * 0.5 * gs;
                    r += x * 0.5 * gs;
                }
                Role::Bl => l += x * 0.5 * gs,
                Role::Br => r += x * 0.5 * gs,
                Role::Lfe => {
                    l += x * glfe;
                    r += x * glfe;
                }
                Role::Skip => {}
            }
        }
        out.push(l * scale);
        out.push(r * scale);
    }
}

/// What a backend produced: interleaved stereo at `rate`.
trait Backend: Send {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> u32;
    fn reset(&mut self);
}

// ------------------------------------------------------------------------------------ AC-3 / E-AC-3
struct Ac3 {
    dec: Box<dyn Decoder>,
    eac3: bool,
    rate: u32,
    fails: u32,
}

impl Ac3 {
    fn new(eac3: bool, rate: u32) -> Result<Ac3, String> {
        let mut p = CodecParameters::audio(CodecId::new(if eac3 { "eac3" } else { "ac3" }));
        p.channels = Some(2);
        p.sample_rate = Some(rate);
        let dec = if eac3 { oxideav_ac3::decoder::make_eac3_decoder(&p) } else { oxideav_ac3::decoder::make_decoder(&p) };
        Ok(Ac3 { dec: dec.map_err(|e| format!("{e:?}"))?, eac3, rate, fails: 0 })
    }
}

impl Backend for Ac3 {
    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> u32 {
        for unit in split_ac3(data, self.eac3) {
            let p = Packet::new(0, TimeBase::new(1, self.rate as i64), unit.to_vec());
            if let Err(e) = self.dec.send_packet(&p) {
                self.fails += 1;
                if self.fails <= 5 {
                    logln!("ac3 decode error: {e:?}");
                }
                continue;
            }
            loop {
                match self.dec.receive_frame() {
                    Ok(Frame::Audio(f)) => {
                        if let Some(plane) = f.data.first() {
                            for c in plane.chunks_exact(2) {
                                out.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0);
                            }
                        }
                    }
                    _ => break,
                }
            }
        }
        if out.len() % 2 == 1 {
            out.pop();
        }
        self.rate
    }
    fn reset(&mut self) {
        let _ = self.dec.reset();
    }
}

/// AC-3: one sync frame each. E-AC-3: one access unit each (independent frame + its dependent frames).
fn split_ac3(data: &[u8], eac3: bool) -> Vec<&[u8]> {
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

// ------------------------------------------------------------------------------------ DTS core
struct Dts {
    dec: Box<dyn Decoder>,
    fails: u32,
}

/// Gain that brings the DTS core decoder output (S32) to full scale; calibrated against ffmpeg.
const DTS_SCALE: f32 = 1.0 / 2147483648.0;

impl Dts {
    fn new() -> Result<Dts, String> {
        let p = CodecParameters::audio(CodecId::new("dts"));
        Ok(Dts { dec: oxideav_dts::make_decoder(&p).map_err(|e| format!("{e:?}"))?, fails: 0 })
    }
}

fn dts_roles(amode: &oxideav_dts::AmodeArrangement, planes: usize) -> Vec<Role> {
    use oxideav_dts::AmodeArrangement as A;
    use Role::*;
    let mut v = match amode {
        A::Mono => vec![C],
        A::DualMono | A::Stereo | A::SumDifference | A::LtRt => vec![L, R],
        A::ClR => vec![C, L, R],
        A::LrS => vec![L, R, S],
        A::ClRS => vec![C, L, R, S],
        A::LrSlSr => vec![L, R, Ls, Rs],
        A::ClRSlSr => vec![C, L, R, Ls, Rs],
        A::ClCrLRSlSr => vec![L, R, L, R, Ls, Rs].into_iter().enumerate().map(|(i, r)| if i < 2 { C } else { r }).collect(),
        A::ClRLrRrOv => vec![C, L, R, Bl, Br, Skip],
        A::CfCrLfRfLrRr => vec![C, Skip, L, R, Bl, Br],
        A::ClCCrLRSlSr => vec![C, C, C, L, R, Ls, Rs],
        _ => vec![L, R],
    };
    v.resize(planes.saturating_sub(0), Skip);
    // an extra trailing plane is the LFE channel
    v
}

impl Backend for Dts {
    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> u32 {
        let hdr = match oxideav_dts::parse_frame_header(data) {
            Ok(h) => h,
            Err(_) => return 48000,
        };
        let rate = hdr.sample_rate_hz().unwrap_or(48000);
        let p = Packet::new(0, TimeBase::new(1, rate as i64), data.to_vec());
        if let Err(e) = self.dec.send_packet(&p) {
            self.fails += 1;
            if self.fails <= 5 {
                logln!("dts decode error: {e:?}");
            }
            return rate;
        }
        match self.dec.receive_frame() {
            Ok(Frame::Audio(f)) => {
                let planes: Vec<Vec<f32>> = f
                    .data
                    .iter()
                    .map(|b| b.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 * DTS_SCALE).collect())
                    .collect();
                let primary = hdr.channel_count().unwrap_or(planes.len() as u8) as usize;
                let mut roles = dts_roles(&hdr.amode_arrangement(), primary.min(planes.len()));
                while roles.len() < planes.len() {
                    roles.push(Role::Lfe);
                }
                mixdown(&planes, &roles, out);
            }
            Err(e) => {
                self.fails += 1;
                if self.fails <= 5 {
                    logln!("dts frame error: {e:?}");
                }
            }
            _ => {}
        }
        rate
    }
    fn reset(&mut self) {
        let _ = self.dec.reset();
    }
}

// ------------------------------------------------------------------------------------ TrueHD
struct TrueHd {
    ex: truehd::process::extract::Extractor,
    parser: truehd::process::parse::Parser,
    dec: truehd::process::decode::Decoder,
    rate: u32,
    fails: u32,
    logged: bool,
}

impl TrueHd {
    fn new() -> TrueHd {
        TrueHd { ex: Default::default(), parser: Default::default(), dec: Default::default(), rate: 48000, fails: 0, logged: false }
    }
}

impl TrueHd {
    /// After an error the filter state is unreliable: start over at the next major sync.
    fn resync(&mut self) {
        self.parser.reset_for_next_major_sync();
        self.dec.reset_for_next_major_sync();
    }
}

fn thd_role(l: &truehd::structs::channel::ChannelLabel) -> Role {
    use truehd::structs::channel::ChannelLabel as C;
    match l {
        C::L | C::Lw => Role::L,
        C::R | C::Rw => Role::R,
        C::C | C::Tfc => Role::C,
        C::LFE | C::LFE2 => Role::Lfe,
        C::Ls | C::Lsd | C::Lsc => Role::Ls,
        C::Rs | C::Rsd | C::Rsc => Role::Rs,
        C::Lb => Role::Bl,
        C::Rb => Role::Br,
        C::Cb => Role::S,
        _ => Role::Skip,
    }
}

impl Backend for TrueHd {
    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> u32 {
        self.ex.push_bytes(data);
        while let Some(frame) = self.ex.next() {
            let Ok(frame) = frame else { continue };
            let mut au = match self.parser.parse(&frame) {
                Ok(a) => a,
                Err(e) => {
                    self.fails += 1;
                    if self.fails <= 5 {
                        logln!("truehd parse error: {e}");
                    }
                    self.resync();
                    continue;
                }
            };
            // Atmos object metadata is not used for a stereo / bed mix; skipping it keeps odd
            // Atmos extension data from failing whole access units
            au.extra_data = None;
            match self.dec.decode_presentation(&au, 0) {
                Ok(d) => {
                    self.rate = d.sampling_frequency.max(8000);
                    if self.fails == 0 && d.sample_length > 0 && !self.logged {
                        self.logged = true;
                        logln!("truehd: presentation 0 = {} channels at {} Hz", d.channel_count, d.sampling_frequency);
                    }
                    let n = d.channel_count.min(16);
                    let roles: Vec<Role> = d.channel_labels.iter().map(thd_role).chain(std::iter::repeat(Role::Skip)).take(n).collect();
                    let planes: Vec<Vec<f32>> = (0..n).map(|c| (0..d.sample_length).map(|s| d.pcm_data[s][c] as f32 / 8388608.0).collect()).collect();
                    mixdown(&planes, &roles, out);
                }
                Err(e) => {
                    self.fails += 1;
                    if self.fails <= 5 {
                        logln!("truehd decode error: {e}");
                    }
                    self.resync();
                }
            }
        }
        self.rate
    }
    fn reset(&mut self) {
        *self = TrueHd::new();
    }
}

// ------------------------------------------------------------------------------------ Opus
/// Several elementary streams (channel mapping family 1/2): the crate's own multistream decoder
/// expects a framing that real files do not use, so the streams are split here and decoded one by one.
struct OpusMulti {
    decs: Vec<opus_decoder::OpusDecoder>,
    coupled: usize,
    mapping: Vec<u8>,
}

enum OpusInner {
    Single(opus_decoder::OpusDecoder),
    Multi(OpusMulti),
}

/// Length field of Opus framing: 1 byte (<252) or 2 bytes. Returns (value, bytes used).
fn opus_len(d: &[u8]) -> Option<(usize, usize)> {
    let a = *d.first()? as usize;
    if a < 252 {
        Some((a, 1))
    } else {
        Some((a + 4 * *d.get(1)? as usize, 2))
    }
}

/// Split a multistream packet: every stream but the last is "self-delimited" (an extra explicit
/// length for the last frame, RFC 6716 appendix B). Returns ordinary packets, one per stream.
fn split_selfdelimited(pkt: &[u8], streams: usize) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(streams);
    let mut p = 0usize;
    for _ in 0..streams.saturating_sub(1) {
        let d = pkt.get(p..)?;
        let toc = *d.first()?;
        let mut std_pkt = vec![toc];
        let mut i = 1usize;
        let (data_len, pad);
        match toc & 3 {
            0 => {
                let (n, u) = opus_len(d.get(i..)?)?;
                i += u;
                data_len = n;
                pad = 0;
            }
            1 => {
                let (n, u) = opus_len(d.get(i..)?)?;
                i += u;
                data_len = 2 * n;
                pad = 0;
            }
            2 => {
                let (n1, u1) = opus_len(d.get(i..)?)?;
                std_pkt.extend_from_slice(d.get(i..i + u1)?);
                i += u1;
                let (n2, u2) = opus_len(d.get(i..)?)?;
                i += u2;
                data_len = n1 + n2;
                pad = 0;
            }
            _ => {
                let c = *d.get(i)?;
                std_pkt.push(c);
                i += 1;
                let (m, vbr, has_pad) = ((c & 0x3F) as usize, c & 0x40 != 0, c & 0x80 != 0);
                let mut padding = 0usize;
                if has_pad {
                    loop {
                        let b = *d.get(i)?;
                        std_pkt.push(b);
                        i += 1;
                        if b == 255 {
                            padding += 254;
                        } else {
                            padding += b as usize;
                            break;
                        }
                    }
                }
                let mut total = 0usize;
                if vbr {
                    for _ in 0..m.saturating_sub(1) {
                        let (n, u) = opus_len(d.get(i..)?)?;
                        std_pkt.extend_from_slice(d.get(i..i + u)?);
                        i += u;
                        total += n;
                    }
                    let (n, u) = opus_len(d.get(i..)?)?;
                    i += u;
                    total += n;
                } else {
                    let (n, u) = opus_len(d.get(i..)?)?;
                    i += u;
                    total += n * m;
                }
                data_len = total;
                pad = padding;
            }
        }
        std_pkt.extend_from_slice(d.get(i..i + data_len + pad)?);
        i += data_len + pad;
        p += i;
        out.push(std_pkt);
    }
    out.push(pkt.get(p..)?.to_vec());
    Some(out)
}

struct Opus {
    inner: OpusInner,
    channels: usize,
    skip: usize,
    preskip: usize,
    buf: Vec<f32>,
    fails: u32,
}

impl Opus {
    /// `private` = OpusHead
    fn new(private: &[u8], channels_hint: usize) -> Result<Opus, String> {
        let (mut channels, mut preskip, mut family) = (channels_hint.max(1), 312usize, 0u8);
        let (mut streams, mut coupled, mut mapping) = (1usize, usize::from(channels_hint > 1), Vec::new());
        if private.len() >= 19 && &private[..8] == b"OpusHead" {
            channels = private[9] as usize;
            preskip = u16::from_le_bytes([private[10], private[11]]) as usize;
            family = private[18];
            if family != 0 && private.len() >= 21 + channels {
                streams = private[19] as usize;
                coupled = private[20] as usize;
                mapping = private[21..21 + channels].to_vec();
            }
        }
        let inner = if family == 0 || mapping.is_empty() {
            let ch = channels.clamp(1, 2);
            channels = ch;
            OpusInner::Single(opus_decoder::OpusDecoder::new(48000, ch).map_err(|e| format!("{e:?}"))?)
        } else {
            let mut decs = Vec::new();
            for i in 0..streams {
                decs.push(opus_decoder::OpusDecoder::new(48000, if i < coupled { 2 } else { 1 }).map_err(|e| format!("{e:?}"))?);
            }
            OpusInner::Multi(OpusMulti { decs, coupled, mapping: mapping.clone() })
        };
        Ok(Opus { inner, channels, skip: preskip, preskip, buf: vec![0.0; 5760 * channels.max(1)], fails: 0 })
    }
}

fn decode_multi(m: &mut OpusMulti, data: &[u8], out: &mut [f32], channels: usize) -> Result<usize, opus_decoder::OpusError> {
    use opus_decoder::OpusError;
    let parts = split_selfdelimited(data, m.decs.len()).ok_or(OpusError::InvalidPacket)?;
    let mut pcm: Vec<Vec<f32>> = Vec::with_capacity(parts.len());
    let mut frames = 0usize;
    for (i, (dec, part)) in m.decs.iter_mut().zip(&parts).enumerate() {
        let ch = if i < m.coupled { 2 } else { 1 };
        let mut buf = vec![0.0f32; 5760 * ch];
        let n = dec.decode_float(part, &mut buf, false)?;
        if i > 0 && n != frames {
            return Err(OpusError::InvalidPacket);
        }
        frames = n;
        buf.truncate(n * ch);
        pcm.push(buf);
    }
    if out.len() < frames * channels {
        return Err(OpusError::BufferTooSmall);
    }
    for (c, &slot) in m.mapping.iter().enumerate().take(channels) {
        for f in 0..frames {
            out[f * channels + c] = if slot == 255 {
                0.0
            } else {
                let slot = slot as usize;
                let (st, sc) = if slot < 2 * m.coupled { (slot / 2, slot % 2) } else { (m.coupled + slot - 2 * m.coupled, 0) };
                let ch = if st < m.coupled { 2 } else { 1 };
                pcm.get(st).map_or(0.0, |p| p[f * ch + sc])
            };
        }
    }
    Ok(frames)
}

impl Backend for Opus {
    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> u32 {
        let n = match &mut self.inner {
            OpusInner::Single(d) => d.decode_float(data, &mut self.buf, false),
            OpusInner::Multi(m) => decode_multi(m, data, &mut self.buf, self.channels),
        };
        let n = match n {
            Ok(n) => n,
            Err(e) => {
                self.fails += 1;
                if self.fails <= 5 {
                    logln!("opus decode error: {e:?}");
                }
                return 48000;
            }
        };
        let ch = self.channels;
        let mut start = 0;
        if self.skip > 0 {
            let s = self.skip.min(n);
            self.skip -= s;
            start = s;
        }
        let planes: Vec<Vec<f32>> = (0..ch).map(|c| (start..n).map(|i| self.buf[i * ch + c]).collect()).collect();
        let roles = vorbis_roles(ch);
        mixdown(&planes, &roles, out);
        48000
    }
    fn reset(&mut self) {
        match &mut self.inner {
            OpusInner::Single(d) => d.reset(),
            OpusInner::Multi(m) => m.decs.iter_mut().for_each(|d| d.reset()),
        }
        self.skip = self.preskip.min(0);
    }
}

// ------------------------------------------------------------------------------------ Vorbis
struct Vorbis {
    dec: symphonia_codec_vorbis::VorbisDecoder,
    rate: u32,
    pts: i64,
    tmp: Vec<f32>,
}

impl Vorbis {
    fn new(private: &[u8]) -> Result<Vorbis, String> {
        use symphonia_core::codecs::audio::well_known::CODEC_ID_VORBIS;
        use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_VORBIS);
        p.extra_data = Some(private.to_vec().into_boxed_slice());
        let dec = symphonia_codec_vorbis::VorbisDecoder::try_new(&p, &AudioDecoderOptions::default()).map_err(|e| format!("{e:?}"))?;
        Ok(Vorbis { dec, rate: 48000, pts: 0, tmp: Vec::new() })
    }
}

impl Backend for Vorbis {
    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> u32 {
        use symphonia_core::codecs::audio::AudioDecoder;
        use symphonia_core::packet::PacketRef;
        use symphonia_core::units::{Duration, Timestamp};
        let pkt = PacketRef::new(0, Timestamp::new(self.pts), Duration::new(0), data);
        self.pts += 1;
        match self.dec.decode_ref(&pkt) {
            Ok(buf) => {
                let spec = buf.spec();
                self.rate = spec.rate();
                let ch = spec.channels().count();
                self.tmp.clear();
                buf.copy_to_vec_interleaved::<f32>(&mut self.tmp);
                let frames = self.tmp.len() / ch.max(1);
                let planes: Vec<Vec<f32>> = (0..ch).map(|c| (0..frames).map(|i| self.tmp[i * ch + c]).collect()).collect();
                mixdown(&planes, &vorbis_roles(ch), out);
            }
            Err(_) => {}
        }
        self.rate
    }
    fn reset(&mut self) {
        use symphonia_core::codecs::audio::AudioDecoder;
        self.dec.reset();
    }
}

// ------------------------------------------------------------------------------------ resampler
/// Streaming windowed-sinc resampler (stereo). Bypassed when the rates match.
struct Resampler {
    in_rate: u32,
    out_rate: u32,
    step: f64,
    half: usize,
    table: Vec<f32>,
    buf: Vec<f32>,
    pos: f64,
}

const SINC_STEPS: usize = 128;

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Resampler {
        let step = in_rate as f64 / out_rate as f64;
        let fc = (1.0 / step).min(1.0) * 0.97; // cutoff relative to the input Nyquist
        let half = ((16.0 / fc).ceil() as usize).max(16);
        let n = half * SINC_STEPS + 2;
        let mut table = Vec::with_capacity(n);
        for i in 0..n {
            let d = i as f64 / SINC_STEPS as f64;
            let x = d * fc;
            let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let w = (d / half as f64).min(1.0);
            let win = 0.42 + 0.5 * (std::f64::consts::PI * w).cos() + 0.08 * (2.0 * std::f64::consts::PI * w).cos();
            table.push((fc * sinc * win) as f32);
        }
        Resampler { in_rate, out_rate, step, half, table, buf: Vec::new(), pos: 0.0 }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let n = self.buf.len() / 2;
        let half = self.half as isize;
        while self.pos + (self.half as f64) + 1.0 < n as f64 {
            let c = self.pos.floor() as isize;
            let (mut l, mut r) = (0f32, 0f32);
            for k in (-half + 1)..=half {
                let idx = c + k;
                if idx < 0 {
                    continue;
                }
                let d = (idx as f64 - self.pos).abs() * SINC_STEPS as f64;
                let i = d as usize;
                let f = (d - i as f64) as f32;
                let w = self.table[i] * (1.0 - f) + self.table[i + 1] * f;
                let b = idx as usize * 2;
                l += self.buf[b] * w;
                r += self.buf[b + 1] * w;
            }
            out.push(l);
            out.push(r);
            self.pos += self.step;
        }
        let drop = ((self.pos.floor() as isize) - half - 2).max(0) as usize;
        if drop > 0 {
            self.buf.drain(..drop * 2);
            self.pos -= drop as f64;
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0.0;
    }
}

// ------------------------------------------------------------------------------------ public wrapper
pub struct PureDecoder {
    inner: Box<dyn Backend>,
    rs: Option<Resampler>,
    out_rate: u32,
    scratch: Vec<f32>,
    pub kind: Kind,
}

impl PureDecoder {
    /// `private` = Matroska CodecPrivate (OpusHead, Vorbis headers, ...); `in_rate` is a hint for
    /// codecs that do not report their own rate.
    pub fn new(kind: Kind, private: &[u8], channels: u32, in_rate: u32, out_rate: u32) -> Result<PureDecoder, String> {
        let in_rate = if in_rate == 0 { 48000 } else { in_rate };
        let inner: Box<dyn Backend> = match kind {
            Kind::Ac3 => Box::new(Ac3::new(false, in_rate)?),
            Kind::Eac3 => Box::new(Ac3::new(true, in_rate)?),
            Kind::Dts => Box::new(Dts::new()?),
            Kind::TrueHd => Box::new(TrueHd::new()),
            Kind::Opus => Box::new(Opus::new(private, channels as usize)?),
            Kind::Vorbis => Box::new(Vorbis::new(private)?),
        };
        Ok(PureDecoder { inner, rs: None, out_rate: out_rate.max(8000), scratch: Vec::new(), kind })
    }

    /// Decode one container packet; returns interleaved stereo f32 at the output rate.
    pub fn decode(&mut self, data: &[u8]) -> Vec<f32> {
        self.scratch.clear();
        // third-party decoders must never take the player down: a panic just drops this packet
        let inner = &mut self.inner;
        let scratch = &mut self.scratch;
        let rate = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.decode(data, scratch))) {
            Ok(r) => r,
            Err(_) => {
                logln!("{} decoder panicked; packet dropped", self.kind.name());
                self.scratch.clear();
                return Vec::new();
            }
        };
        if self.scratch.is_empty() {
            return Vec::new();
        }
        if self.rs.as_ref().map_or(true, |r| r.in_rate != rate) {
            self.rs = Some(Resampler::new(rate, self.out_rate));
        }
        let mut out = Vec::with_capacity(self.scratch.len() * self.out_rate as usize / rate.max(1) as usize + 8);
        self.rs.as_mut().unwrap().process(&self.scratch, &mut out);
        out
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        if let Some(r) = self.rs.as_mut() {
            r.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_tone() {
        for (a, b) in [(192000u32, 48000u32), (44100, 48000), (48000, 44100), (96000, 48000)] {
            let mut rs = Resampler::new(a, b);
            let n = a as usize; // 1 s
            let input: Vec<f32> = (0..n).flat_map(|i| { let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / a as f32).sin() * 0.5; [s, s] }).collect();
            let mut out = Vec::new();
            for chunk in input.chunks(2 * 1536) {
                rs.process(chunk, &mut out);
            }
            let frames = out.len() / 2;
            assert!((frames as i64 - b as i64).abs() < 200, "{a}->{b}: {frames}");
            let tail = &out[out.len() / 2..];
            let peak = tail.iter().fold(0f32, |m, x| m.max(x.abs()));
            assert!((peak - 0.5).abs() < 0.03, "{a}->{b}: peak {peak}");
        }
    }

    /// Decode a generated file through the real MKV demuxer; prints stats (dev aid, skipped when the file is absent).
    fn run_file(path: &str) -> Option<(usize, f32, f32, f32, f32)> {
        if !std::path::Path::new(path).exists() {
            return None;
        }
        let tracks = crate::mkv::read_tracks(path)?;
        let t = tracks.iter().find(|t| t.ttype == 2)?;
        let kind = Kind::from_mkv(&t.codec)?;
        let mut dec = PureDecoder::new(kind, &t.private, t.channels as u32, t.sample_rate as u32, 48000).unwrap();
        let mut dm = crate::mkvaudio::Demux::open(path, t.number)?;
        let mut out = Vec::new();
        while let Some((_, pkt)) = dm.next_packet() {
            out.extend(dec.decode(&pkt));
        }
        let frames = out.len() / 2;
        let rms = |c: usize| (out.iter().skip(c).step_by(2).map(|x| x * x).sum::<f32>() / frames.max(1) as f32).sqrt();
        let zc = |c: usize| {
            let v: Vec<f32> = out.iter().skip(c).step_by(2).copied().collect();
            let z = v.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
            z as f32 / (frames as f32 / 48000.0)
        };
        eprintln!("{path}: {:?} frames={frames} rmsL={} rmsR={} fL={} fR={}", kind.name(), rms(0), rms(1), zc(0), zc(1));
        Some((frames, rms(0), rms(1), zc(0), zc(1)))
    }

    #[test]
    fn decodes_generated_files() {
        for f in ["dts51", "thd", "op", "vb", "ac3", "eac3", "thd51", "vb51", "dts441", "opmono", "op40", "op51"] {
            if let Some((frames, rl, rr, fl, fr)) = run_file(&format!("/tmp/claude-0/a/{f}.mkv")) {
                assert!(frames > 48000 * 2, "{f}: only {frames} frames");
                assert!(rl > 0.05 && rr > 0.05, "{f}: silent {rl} {rr}");
                assert!(rl < 1.0 && rr < 1.0, "{f}: too loud");
                let _ = (fl, fr);
            }
        }
    }
}
