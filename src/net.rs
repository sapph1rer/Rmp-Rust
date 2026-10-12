//! Local loopback server that lets Media Foundation (which cannot send custom
//! HTTP headers) play protected links:
//!
//!  * HLS (`.m3u8`, or playlists with odd extensions like `.txt`): the segments
//!    are presented as ONE seekable MPEG-TS file of known length; segments are
//!    fetched on demand (with the Referer/User-Agent/headers) and prefetched.
//!  * Plain progressive URLs: Range requests are forwarded with the headers.
//!
//! The player then opens `http://127.0.0.1:<port>/stream.ts|.mp4|...` like any file.

use crate::player::Options;
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------ upstream

#[derive(Clone)]
struct Upstream {
    agent: ureq::Agent,
    headers: Vec<(String, String)>,
}

impl Upstream {
    fn new(opts: &Options) -> Upstream {
        let ua = opts.user_agent.clone().unwrap_or_else(|| "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36".into());
        #[allow(unused_mut)]
        let mut b = ureq::AgentBuilder::new()
            .user_agent(&ua)
            .timeout_connect(Duration::from_secs(12))
            .timeout_read(Duration::from_secs(40))
            .redirects(8);
        #[cfg(windows)]
        {
            if let Ok(tls) = native_tls::TlsConnector::new() {
                b = b.tls_connector(Arc::new(tls));
            }
        }
        let mut headers = Vec::new();
        if let Some(r) = &opts.referrer {
            headers.push(("Referer".to_string(), r.clone()));
        }
        for h in &opts.headers {
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        Upstream { agent: b.build(), headers }
    }

    fn req(&self, method: &str, url: &str) -> ureq::Request {
        let mut r = self.agent.request(method, url);
        for (k, v) in &self.headers {
            r = r.set(k, v);
        }
        r
    }

    fn get_range(&self, url: &str, range: Option<(u64, Option<u64>)>) -> Result<ureq::Response> {
        let mut r = self.req("GET", url);
        if let Some((a, b)) = range {
            r = r.set("Range", &match b {
                Some(b) => format!("bytes={a}-{b}"),
                None => format!("bytes={a}-"),
            });
        }
        r.call().map_err(|e| anyhow!("GET {url}: {e}"))
    }
}

fn parse_content_range_total(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

// ----------------------------------------------------------------- url utils

fn origin(url: &str) -> &str {
    let after = url.find("://").map(|i| i + 3).unwrap_or(0);
    match url[after..].find('/') {
        Some(i) => &url[..after + i],
        None => url,
    }
}

fn join(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    if let Some(r) = rel.strip_prefix("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}://{r}");
    }
    if rel.starts_with('/') {
        return format!("{}{rel}", origin(base));
    }
    let no_q = base.split(['?', '#']).next().unwrap_or(base);
    let after = no_q.find("://").map(|x| x + 3).unwrap_or(0);
    let dir = match no_q[after..].rfind('/') {
        Some(i) => no_q[..after + i + 1].to_string(),
        None => format!("{no_q}/"),
    };
    format!("{dir}{rel}")
}

fn path_ext(url: &str) -> String {
    let p = url.split(['?', '#']).next().unwrap_or("");
    let name = p.rsplit('/').next().unwrap_or("");
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "ts" | "m2ts" => "video/mp2t",
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

// ----------------------------------------------------------------------- HLS

enum Playlist {
    Master(Vec<(u64, String)>),
    Media(Vec<String>),
}

fn parse_playlist(base: &str, body: &str) -> Result<Playlist> {
    if !body.trim_start().starts_with("#EXTM3U") {
        bail!("not an HLS playlist");
    }
    let mut variants: Vec<(u64, String)> = Vec::new();
    let mut segs: Vec<String> = Vec::new();
    let mut pending_bw: Option<u64> = None;
    let mut expect_seg = false;
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bw = a.split(',').find_map(|p| p.trim().strip_prefix("BANDWIDTH=")).and_then(|v| v.parse().ok()).unwrap_or(0);
            pending_bw = Some(bw);
        } else if line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE") {
            bail!("encrypted HLS (EXT-X-KEY) is not supported yet");
        } else if line.starts_with("#EXT-X-MAP:") {
            bail!("fMP4/CMAF HLS (EXT-X-MAP) is not supported yet");
        } else if line.starts_with("#EXTINF:") {
            expect_seg = true;
        } else if line.starts_with('#') {
            continue;
        } else if let Some(bw) = pending_bw.take() {
            variants.push((bw, join(base, line)));
        } else if expect_seg || !variants.is_empty() {
            expect_seg = false;
            segs.push(join(base, line));
        } else {
            segs.push(join(base, line));
        }
    }
    if !variants.is_empty() && segs.is_empty() {
        return Ok(Playlist::Master(variants));
    }
    if segs.is_empty() {
        bail!("playlist has no segments");
    }
    Ok(Playlist::Media(segs))
}

/// Offset of the first of 3 consecutive MPEG-TS sync bytes (skips fake image/html headers).
fn ts_prefix(data: &[u8]) -> usize {
    let limit = data.len().saturating_sub(188 * 2 + 1).min(4096);
    (0..=limit).find(|&o| data.get(o) == Some(&0x47) && data.get(o + 188) == Some(&0x47) && data.get(o + 376) == Some(&0x47)).unwrap_or(0)
}

struct Seg {
    url: String,
    start: u64, // virtual offset
    len: u64,   // length after prefix removal
    prefix: usize,
}

enum Slot {
    Loading,
    Ready(Arc<Vec<u8>>),
}

struct Hls {
    up: Upstream,
    segs: Vec<Seg>,
    total: u64,
    cache: Mutex<HashMap<usize, Slot>>,
    cv: Condvar,
    order: Mutex<Vec<usize>>,
}

impl Hls {
    fn segment(self: &Arc<Self>, i: usize) -> Result<Arc<Vec<u8>>> {
        let mut c = self.cache.lock().unwrap();
        loop {
            match c.get(&i) {
                Some(Slot::Ready(d)) => return Ok(d.clone()),
                Some(Slot::Loading) => c = self.cv.wait(c).unwrap(),
                None => break,
            }
        }
        c.insert(i, Slot::Loading);
        drop(c);
        let res = self.download(i);
        let mut c = self.cache.lock().unwrap();
        match res {
            Ok(d) => {
                let d = Arc::new(d);
                c.insert(i, Slot::Ready(d.clone()));
                let mut o = self.order.lock().unwrap();
                o.push(i);
                while o.len() > 14 {
                    let old = o.remove(0);
                    c.remove(&old);
                }
                self.cv.notify_all();
                Ok(d)
            }
            Err(e) => {
                c.remove(&i);
                self.cv.notify_all();
                Err(e)
            }
        }
    }

    fn download(&self, i: usize) -> Result<Vec<u8>> {
        let seg = &self.segs[i];
        let mut last = anyhow!("no attempt");
        for attempt in 0..3 {
            match self.up.get_range(&seg.url, None) {
                Ok(resp) => {
                    let mut buf = Vec::with_capacity(seg.len as usize + seg.prefix);
                    if let Err(e) = resp.into_reader().take(256 * 1024 * 1024).read_to_end(&mut buf) {
                        last = anyhow!("read {}: {e}", seg.url);
                    } else {
                        let mut out = buf.split_off(seg.prefix.min(buf.len()));
                        out.resize(seg.len as usize, 0); // keep the virtual layout intact
                        return Ok(out);
                    }
                }
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
        }
        Err(last)
    }

    fn prefetch(self: &Arc<Self>, from: usize) {
        for j in from..(from + 3).min(self.segs.len()) {
            let present = self.cache.lock().unwrap().contains_key(&j);
            if !present {
                let me = self.clone();
                std::thread::spawn(move || {
                    let _ = me.segment(j);
                });
            }
        }
    }

    fn write_range(self: &Arc<Self>, out: &mut impl Write, mut pos: u64, end_incl: u64) -> Result<()> {
        // binary search the first segment containing `pos`
        let mut i = match self.segs.binary_search_by(|s| s.start.cmp(&pos)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        while pos <= end_incl && i < self.segs.len() {
            let s = &self.segs[i];
            let data = self.segment(i)?;
            self.prefetch(i + 1);
            let a = (pos - s.start) as usize;
            let b = ((end_incl - s.start + 1).min(s.len)) as usize;
            out.write_all(&data[a..b])?;
            pos = s.start + b as u64;
            i += 1;
        }
        Ok(())
    }
}

fn build_hls(up: &Upstream, url: &str, first_body: Option<String>) -> Result<Arc<Hls>> {
    let mut cur = url.to_string();
    let mut body = match first_body {
        Some(b) => b,
        None => up.get_range(&cur, None)?.into_string()?,
    };
    let mut segs_urls = None;
    for _ in 0..3 {
        match parse_playlist(&cur, &body)? {
            Playlist::Media(s) => {
                segs_urls = Some(s);
                break;
            }
            Playlist::Master(mut v) => {
                v.sort_by_key(|x| std::cmp::Reverse(x.0));
                cur = v[0].1.clone();
                body = up.get_range(&cur, None)?.into_string()?;
            }
        }
    }
    let urls = segs_urls.ok_or_else(|| anyhow!("could not resolve a media playlist"))?;

    // probe every segment (size + fake-header prefix) with small ranged requests, 12 at a time
    let probed: Vec<Result<(u64, usize)>> = {
        let mut out: Vec<Result<(u64, usize)>> = Vec::with_capacity(urls.len());
        for chunk in urls.chunks(12) {
            let res: Vec<Result<(u64, usize)>> = std::thread::scope(|s| {
                let hs: Vec<_> = chunk
                    .iter()
                    .map(|u| {
                        s.spawn(move || -> Result<(u64, usize)> {
                            let resp = up.get_range(u, Some((0, Some(4095))))?;
                            let total = if resp.status() == 206 {
                                resp.header("Content-Range").and_then(parse_content_range_total)
                            } else {
                                resp.header("Content-Length").and_then(|v| v.parse().ok())
                            };
                            let mut head = Vec::new();
                            resp.into_reader().take(4096).read_to_end(&mut head)?;
                            let prefix = ts_prefix(&head);
                            let total = total.ok_or_else(|| anyhow!("no length for {u}"))?;
                            Ok((total, prefix))
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("probe thread panicked")))).collect()
            });
            out.extend(res);
        }
        out
    };
    let mut segs = Vec::new();
    let mut start = 0u64;
    for (u, p) in urls.into_iter().zip(probed) {
        let (total, prefix) = p?;
        let len = total.saturating_sub(prefix as u64);
        segs.push(Seg { url: u, start, len, prefix });
        start += len;
    }
    Ok(Arc::new(Hls { up: up.clone(), segs, total: start, cache: Mutex::new(HashMap::new()), cv: Condvar::new(), order: Mutex::new(Vec::new()) }))
}

// ----------------------------------------------------------- progressive proxy

struct Prog {
    up: Upstream,
    url: String,
    total: u64,
}

fn probe_progressive(up: &Upstream, url: &str) -> Result<u64> {
    let resp = up.get_range(url, Some((0, Some(0))))?;
    let total = if resp.status() == 206 {
        resp.header("Content-Range").and_then(parse_content_range_total)
    } else {
        resp.header("Content-Length").and_then(|v| v.parse().ok())
    };
    total.ok_or_else(|| anyhow!("server sent no length for {url}"))
}

// ------------------------------------------------------------------- server

enum Source {
    Hls(Arc<Hls>),
    Prog(Arc<Prog>),
}

fn parse_range(h: &str, total: u64) -> Option<(u64, u64)> {
    let r = h.trim().strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return Some((total.saturating_sub(n), total.saturating_sub(1)));
    }
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { total.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(total.saturating_sub(1)) };
    if a > b || a >= total { None } else { Some((a, b)) }
}

fn handle(mut s: TcpStream, src: &Source, ctype: &str, verbose: bool) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut rd = BufReader::new(s.try_clone()?);
    let mut first = String::new();
    rd.read_line(&mut first)?;
    let method = first.split_whitespace().next().unwrap_or("GET").to_string();
    let mut range = None;
    loop {
        let mut l = String::new();
        if rd.read_line(&mut l)? == 0 || l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }
    let total = match src {
        Source::Hls(h) => h.total,
        Source::Prog(p) => p.total,
    };
    let (a, b, partial) = match range.as_deref().and_then(|r| parse_range(r, total)) {
        Some((a, b)) => (a, b, true),
        None if range.is_some() => {
            write!(s, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
        None => (0, total.saturating_sub(1), false),
    };
    if verbose {
        eprintln!("[proxy] {method} {a}-{b}/{total}");
    }
    let len = if total == 0 { 0 } else { b - a + 1 };
    if partial {
        write!(s, "HTTP/1.1 206 Partial Content\r\nContent-Type: {ctype}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {a}-{b}/{total}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")?;
    } else {
        write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nAccept-Ranges: bytes\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")?;
    }
    if method == "HEAD" || total == 0 {
        return Ok(());
    }
    match src {
        Source::Hls(h) => h.write_range(&mut s, a, b)?,
        Source::Prog(p) => {
            let resp = p.up.get_range(&p.url, Some((a, Some(b))))?;
            let mut r = resp.into_reader();
            if resp_status_is_full(&mut r, a)? {}
            std::io::copy(&mut r.take(len), &mut s)?;
        }
    }
    s.flush()?;
    Ok(())
}

// If an origin ignores Range and answers 200 with the whole file, the caller of
// `get_range(a..)` would receive bytes from 0; we cannot tell here, so servers that
// ignore Range are only supported for a == 0 (checked when serving starts).
fn resp_status_is_full(_r: &mut dyn Read, _a: u64) -> Result<bool> {
    Ok(false)
}

/// Prepare a local URL for `url`. Returns the loopback URL to hand to the decoder.
pub fn serve(url: &str, opts: &Options, quit: Arc<AtomicBool>, verbose: bool) -> Result<String> {
    let up = Upstream::new(opts);
    let ext = path_ext(url);
    let forced_hls = opts.force_format.as_deref() == Some("hls");

    // decide HLS vs progressive
    let mut first_body = None;
    let mut is_hls = forced_hls || ext == "m3u8";
    if !is_hls && matches!(ext.as_str(), "txt" | "php" | "html" | "") {
        if let Ok(resp) = up.get_range(url, Some((0, Some(511)))) {
            let mut head = Vec::new();
            let _ = resp.into_reader().take(512).read_to_end(&mut head);
            if head.starts_with(b"#EXTM3U") {
                is_hls = true;
            }
        }
    }
    let (src, ctype, local_ext) = if is_hls {
        let _ = &mut first_body;
        let h = build_hls(&up, url, first_body)?;
        if verbose {
            eprintln!("[proxy] HLS: {} segments, {} bytes virtual", h.segs.len(), h.total);
        }
        (Source::Hls(h), "video/mp2t", "ts".to_string())
    } else {
        let total = probe_progressive(&up, url)?;
        let e = if ext.is_empty() || ext.len() > 5 { "mp4".to_string() } else { ext };
        let ct = content_type(&e);
        (Source::Prog(Arc::new(Prog { up, url: url.to_string(), total })), ct, e)
    };

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let src = Arc::new(src);
    std::thread::Builder::new().name("proxy".into()).spawn(move || {
        while !quit.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let src = src.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle(s, &src, ctype, verbose) {
                            if verbose {
                                eprintln!("[proxy] request ended: {e:#}");
                            }
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(15)),
                Err(_) => break,
            }
        }
    })?;
    Ok(format!("http://127.0.0.1:{port}/stream.{local_ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        assert_eq!(join("https://a.com/x/y/p.m3u8?t=1", "s1.ts"), "https://a.com/x/y/s1.ts");
        assert_eq!(join("https://a.com/x/p.m3u8", "/s/1.ts"), "https://a.com/s/1.ts");
        assert_eq!(join("https://a.com/x/p.m3u8", "//cdn.b.com/1.ts"), "https://cdn.b.com/1.ts");
        assert_eq!(join("https://a.com", "s.ts"), "https://a.com/s.ts");
        assert_eq!(path_ext("https://a.com/m3u8/abc-480.txt?x=1"), "txt");
    }
    #[test]
    fn playlist() {
        let b = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.html\n#EXTINF:4.0,\nseg1.html\n#EXT-X-ENDLIST\n";
        match parse_playlist("https://h/m3u8/p.txt", b).unwrap() {
            Playlist::Media(s) => assert_eq!(s, vec!["https://h/m3u8/seg0.html", "https://h/m3u8/seg1.html"]),
            _ => panic!(),
        }
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000\nhi.m3u8\n";
        match parse_playlist("https://h/p.m3u8", m).unwrap() {
            Playlist::Master(v) => assert_eq!(v.len(), 2),
            _ => panic!(),
        }
        assert!(parse_playlist("https://h/p.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\na.ts\n").is_err());
    }
    #[test]
    fn ts_sync() {
        let mut d = vec![0u8; 100];
        for k in 0..5 {
            d.push(0x47);
            d.extend(std::iter::repeat(0).take(187));
            let _ = k;
        }
        assert_eq!(ts_prefix(&d), 100);
    }
    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=200-", 100), None);
    }
}
