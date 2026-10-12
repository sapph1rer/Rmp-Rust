//! Audio for tracks Windows cannot decode (DTS, TrueHD, Opus, Vorbis, AC-3/E-AC-3 ...) in a local
//! Matroska file: our own demuxer (`mkvaudio`) + the pure-Rust decoders (`codecs`), running on a
//! thread of its own and feeding the same audio queue Media Foundation audio uses.

use crate::codecs::{Kind, PureDecoder};
use crate::mkv::Track;
use crate::mkvaudio::Demux;
use crate::player::{AtomicF64, Shared};
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct ExtAudio {
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
    target: Arc<AtomicF64>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// false while the thread is still opening the file (Cues-less files: one pass over all clusters)
    opened: Arc<AtomicBool>,
}

impl ExtAudio {
    /// Start decoding `track` of `path`. `start` = timeline position (seconds) to begin at.
    pub fn spawn(sh: Arc<Shared>, path: &str, track: &Track, out_rate: u32, start: f64) -> Result<ExtAudio, String> {
        let kind = Kind::from_mkv(&track.codec).ok_or_else(|| format!("unsupported codec {}", track.codec))?;
        let dec = PureDecoder::new(kind, &track.private, track.channels as u32, track.sample_rate as u32, out_rate)?;
        let stop = Arc::new(AtomicBool::new(false));
        let target = Arc::new(AtomicF64::new(start.max(0.0)));
        let (stop2, target2) = (stop.clone(), target.clone());
        let (path, number) = (path.to_string(), track.number);
        let opened = Arc::new(AtomicBool::new(false));
        let opened2 = opened.clone();
        let sh_keep = sh.clone();
        let name = kind.name();
        let handle = std::thread::Builder::new()
            .name(format!("audio-{name}"))
            .spawn(move || {
                // opened here, not on the caller's thread: files without Cues need a pass over all clusters
                let dm = Demux::open(&path, number);
                opened2.store(true, SeqCst);
                match dm {
                    Some(_) if stop2.load(SeqCst) => {}
                    Some(dm) => run(sh, dm, dec, stop2, target2),
                    None => {
                        logln!("audio: cannot read track {number} of '{path}'");
                        sh.set_note("No sound: cannot read this audio track");
                        sh.audio_eof.store(true, SeqCst);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(ExtAudio { sh: sh_keep, stop, target, handle: Some(handle), opened })
    }

    /// Call right BEFORE bumping `Shared.serial` for a seek, so the thread sees the new target.
    pub fn set_target(&self, t: f64) {
        self.target.store(t.max(0.0));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(h) = self.handle.take() {
            if !self.opened.load(SeqCst) {
                return; // still opening the file: it ends by itself, do not make the caller wait for it
            }
            // the thread may sit in `push_audio` waiting for room: keep draining until it leaves
            while !h.is_finished() {
                self.sh.audio.lock().unwrap().q.clear();
                self.sh.audio_cv.notify_all();
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = h.join();
        }
    }
}

impl Drop for ExtAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(sh: Arc<Shared>, mut dm: Demux, mut dec: PureDecoder, stop: Arc<AtomicBool>, target: Arc<AtomicF64>) {
    let alive = || !stop.load(Relaxed) && !sh.quit.load(Relaxed);
    // the timeline starts at the first video timestamp; wait until the reader knows it
    let t_wait = Instant::now();
    while alive() && !sh.t0_ready.load(SeqCst) && sh.has_video.load(SeqCst) && t_wait.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let out_rate = sh.out_rate.load(Relaxed).max(1) as f64;
    let mut serial = 0u64; // forces the initial positioning below
    let mut t0 = 0.0;
    let mut tgt = 0.0;
    let mut eof = false;
    let (mut fed, mut got, mut warned) = (0u32, 0u32, false);
    while alive() {
        let s = sh.serial.load(SeqCst);
        if s != serial {
            serial = s;
            tgt = target.load();
            t0 = sh.t0.load();
            dm.seek(tgt + t0);
            dec.reset();
            eof = false;
        }
        if eof {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        let Some((pts, pkt)) = dm.next_packet() else {
            eof = true;
            if !sh.stale(serial) {
                sh.audio_eof.store(true, SeqCst);
            }
            continue;
        };
        let mut pcm = dec.decode(&pkt);
        fed += 1;
        if pcm.is_empty() {
            if fed >= 400 && got == 0 && !warned {
                // many packets and not one sample: this stream cannot be decoded (e.g. an unsupported variant)
                warned = true;
                logln!("audio: {} packets fed to the decoder, no sound produced", fed);
                sh.set_note("No sound: this audio stream could not be decoded - pick another audio track");
            }
            continue;
        }
        got += 1;
        let mut p = pts - t0;
        if p < tgt {
            // packets before the seek target are decoded (codec warm-up) but not played
            let skip = (((tgt - p) * out_rate) as usize * 2).min(pcm.len()) & !1;
            pcm.drain(..skip);
            p = tgt;
            if pcm.is_empty() {
                continue;
            }
        }
        sh.push_audio(serial, p + sh.audio_delay.load(), &pcm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_and_seeks() {
        for f in ["dts51", "thd", "op", "vb"] {
            let path = format!("/tmp/claude-0/a/{f}.mkv");
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            check_file(&path);
        }
    }

    /// same, but over HTTP (needs `python3 /tmp/claude-0/rangesrv.py` on port 8765)
    #[test]
    fn plays_over_http() {
        if std::net::TcpStream::connect("127.0.0.1:8765").is_err() {
            return;
        }
        for f in ["dts51", "op", "vb"] {
            check_file(&format!("http://127.0.0.1:8765/{f}.mkv"));
        }
    }

    fn check_file(path: &str) {
        {
            let f = path;
            let path = path.to_string();
            let tracks = crate::mkv::read_tracks(&path).unwrap();
            let t = tracks.iter().find(|t| t.ttype == 2).unwrap();
            let sh = Arc::new(Shared::new());
            sh.t0_ready.store(true, SeqCst);
            let mut ext = ExtAudio::spawn(sh.clone(), &path, t, 48000, 0.0).unwrap();
            let mut got = 0usize;
            let t_start = Instant::now();
            while !sh.audio_eof.load(SeqCst) && t_start.elapsed() < Duration::from_secs(10) {
                let mut a = sh.audio.lock().unwrap();
                got += a.q.len();
                a.q.clear();
                drop(a);
                sh.audio_cv.notify_all();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(sh.audio_eof.load(SeqCst), "{f}: no EOF");
            let secs = got as f64 / 2.0 / 48000.0;
            assert!((secs - 3.0).abs() < 0.2, "{f}: {secs}s");
            // seek to 2.0 s: expect ~1 s of audio, starting at 2.0
            ext.set_target(2.0);
            sh.audio_eof.store(false, SeqCst);
            {
                let mut a = sh.audio.lock().unwrap();
                a.q.clear();
                a.has_clock = false;
            }
            sh.serial.fetch_add(1, SeqCst);
            let mut got = 0usize;
            let mut first_pts = None;
            let t_start = Instant::now();
            while !sh.audio_eof.load(SeqCst) && t_start.elapsed() < Duration::from_secs(10) {
                let mut a = sh.audio.lock().unwrap();
                if first_pts.is_none() && a.has_clock {
                    first_pts = Some(a.pts_front);
                }
                got += a.q.len();
                a.q.clear();
                drop(a);
                sh.audio_cv.notify_all();
                std::thread::sleep(Duration::from_millis(5));
            }
            let secs = got as f64 / 2.0 / 48000.0;
            eprintln!("{f}: after seek {secs:.2}s first_pts={first_pts:?}");
            assert!((secs - 1.0).abs() < 0.2, "{f}: {secs}s after seek");
            assert!((first_pts.unwrap() - 2.0).abs() < 0.05);
            ext.stop();
        }
    }
}
