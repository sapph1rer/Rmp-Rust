//! Video for codecs Windows cannot decode (HEVC / VP9 / AV1 without the Store extensions) in a
//! Matroska file: own demuxer (`mkvaudio`) + pure-Rust software decoders (`vcodecs`) on one thread,
//! feeding the same frame queue Media Foundation video uses.

use crate::mkv::Track;
use crate::mkvaudio::Demux;
use crate::player::{AtomicF64, Frame, Shared};
use crate::vcodecs::{to_pixels, Picture, VKind, VideoDecoder};
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct SwVideo {
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
    target: Arc<AtomicF64>,
    handle: Option<std::thread::JoinHandle<()>>,
    opened: Arc<AtomicBool>,
}

impl SwVideo {
    pub fn spawn(sh: Arc<Shared>, path: &str, track: &Track, start: f64) -> Result<SwVideo, String> {
        let kind = VKind::from_mkv(&track.codec).ok_or_else(|| format!("no built-in decoder for {}", track.codec))?;
        let dec = VideoDecoder::new(kind, &track.private, track.width, track.height)?;
        let stop = Arc::new(AtomicBool::new(false));
        let target = Arc::new(AtomicF64::new(start.max(0.0)));
        let (stop2, target2, sh2) = (stop.clone(), target.clone(), sh.clone());
        let (path, number) = (path.to_string(), track.number);
        let opened = Arc::new(AtomicBool::new(false));
        let opened2 = opened.clone();
        let handle = std::thread::Builder::new().name(format!("video-{}", kind.name())).spawn(move || {
                let dm = Demux::open(&path, number);
                opened2.store(true, SeqCst);
                match dm {
                Some(_) if stop2.load(SeqCst) => {}
                Some(dm) => run(sh2, dm, dec, stop2, target2),
                None => {
                    logln!("video: cannot read track {number} of '{path}'");
                    sh2.set_note("Cannot read the video track");
                    sh2.video_eof.store(true, SeqCst);
                }
                }
            }).map_err(|e| e.to_string())?;
        Ok(SwVideo { sh, stop, target, handle: Some(handle), opened })
    }

    /// Call right BEFORE bumping `Shared.serial` for a seek.
    pub fn set_target(&self, t: f64) {
        self.target.store(t.max(0.0));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(h) = self.handle.take() {
            if !self.opened.load(SeqCst) {
                return; // still opening the file: it ends by itself
            }
            // the thread may wait in `push_frame` for room: keep draining until it leaves
            while !h.is_finished() {
                self.sh.frames.lock().unwrap().clear();
                self.sh.frames_cv.notify_all();
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = h.join();
        }
    }
}

impl Drop for SwVideo {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(sh: Arc<Shared>, mut dm: Demux, mut dec: VideoDecoder, stop: Arc<AtomicBool>, target: Arc<AtomicF64>) {
    let alive = || !stop.load(Relaxed) && !sh.quit.load(Relaxed);
    let mut serial = 0u64; // forces the initial positioning below
    let (mut tgt, mut seek_at) = (0.0, Instant::now());
    let (mut eof, mut errors) = (false, 0u32);
    let mut pics: Vec<Picture> = Vec::new();
    while alive() {
        let s = sh.serial.load(SeqCst);
        if s != serial {
            serial = s;
            tgt = target.load();
            seek_at = Instant::now();
            dm.seek(tgt);
            dec.reset();
            pics.clear();
            eof = false;
        }
        if eof {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        match dm.next_packet() {
            Some((pts, pkt)) => {
                if let Err(e) = dec.decode(&pkt, pts, &mut pics) {
                    errors += 1;
                    if errors <= 5 {
                        logln!("video decode error: {e}");
                    }
                }
            }
            None => {
                dec.flush(&mut pics);
                eof = true;
            }
        }
        for pic in pics.drain(..) {
            if sh.stale(serial) {
                break;
            }
            // precise seek: skip pictures before the target, but not for long (slow decoders)
            if pic.pts < tgt - 0.001 && seek_at.elapsed() < Duration::from_millis(4000) {
                continue;
            }
            let (w, h) = (pic.w as u32, pic.h as u32);
            let pixels = to_pixels(&pic);
            sh.push_frame(Frame { w, h, pixels, pts: pic.pts, serial });
        }
        if eof && !sh.stale(serial) {
            sh.video_eof.store(true, SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_av1_and_seeks() {
        let path = "/tmp/claude-0/v/av1.mkv";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let t = crate::mkv::read_tracks(path).unwrap().into_iter().find(|t| t.ttype == 1).unwrap();
        let sh = Arc::new(Shared::new());
        let mut v = SwVideo::spawn(sh.clone(), path, &t, 0.0).unwrap();
        let drain = |sh: &Shared, secs: u64| {
            let (mut n, mut first, mut last) = (0, None, 0.0);
            let t = Instant::now();
            while !sh.video_eof.load(SeqCst) && t.elapsed() < Duration::from_secs(secs) {
                let mut q = sh.frames.lock().unwrap();
                while let Some(f) = q.pop_front() {
                    first.get_or_insert(f.pts);
                    last = f.pts;
                    n += 1;
                }
                drop(q);
                sh.frames_cv.notify_all();
                std::thread::sleep(Duration::from_millis(5));
            }
            (n, first, last)
        };
        let (n, first, last) = drain(&sh, 60);
        assert!(sh.video_eof.load(SeqCst), "no EOF after {n} frames");
        assert!(n >= 70, "{n} frames");
        assert_eq!(first, Some(0.0));
        assert!((last - 2.958).abs() < 0.1, "last {last}");
        // seek to 2.0 s
        v.set_target(2.0);
        sh.video_eof.store(false, SeqCst);
        sh.frames.lock().unwrap().clear();
        sh.serial.fetch_add(1, SeqCst);
        let (n, first, _) = drain(&sh, 60);
        assert!(sh.video_eof.load(SeqCst));
        eprintln!("after seek: {n} frames, first {first:?}");
        assert!(n >= 20, "{n} frames after seek");
        assert!(first.unwrap() <= 2.1, "{first:?}"); // (a slow debug decoder may give up skipping early)
        v.stop();
    }
}
