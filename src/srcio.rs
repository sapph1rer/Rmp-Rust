//! A seekable byte source for the Matroska parser: a local file or an HTTP(S) link read with Range
//! requests (lazy: a seek only records the position, the next read opens a request there).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

pub enum Src {
    File(File),
    Http(Http),
}

pub struct Http {
    url: String,
    agent: ureq::Agent,
    len: u64,
    pos: u64,
    body: Option<(Box<dyn Read + Send + Sync>, u64)>, // reader, position it is at
}

pub fn is_url(s: &str) -> bool {
    let l = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

pub(crate) fn agent() -> ureq::Agent {
    #[allow(unused_mut)]
    let mut b = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .timeout_connect(Duration::from_secs(12))
        .timeout_read(Duration::from_secs(30))
        .redirects(8);
    #[cfg(windows)]
    {
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
    }
    b.build()
}

impl Src {
    pub fn open(path: &str) -> Option<Src> {
        if is_url(path) {
            let agent = agent();
            // first request also tells the size (Content-Range: bytes 0-0/TOTAL)
            let r = agent.get(path).set("Range", "bytes=0-0").call().ok()?;
            let len = r
                .header("Content-Range")
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .or_else(|| if r.status() == 200 { r.header("Content-Length").and_then(|v| v.parse().ok()) } else { None })?;
            if r.status() != 206 {
                return None; // the server ignores Range: not usable for random access
            }
            Some(Src::Http(Http { url: path.to_string(), agent, len, pos: 0, body: None }))
        } else {
            File::open(path).ok().map(Src::File)
        }
    }

    pub fn len(&self) -> Option<u64> {
        match self {
            Src::File(f) => f.metadata().ok().map(|m| m.len()),
            Src::Http(h) => Some(h.len),
        }
    }

    pub fn is_http(&self) -> bool {
        matches!(self, Src::Http(_))
    }
}

impl Read for Src {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Src::File(f) => f.read(buf),
            Src::Http(h) => h.read(buf),
        }
    }
}

impl Seek for Src {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        match self {
            Src::File(f) => f.seek(p),
            Src::Http(h) => {
                let np = match p {
                    SeekFrom::Start(n) => n as i128,
                    SeekFrom::Current(d) => h.pos as i128 + d as i128,
                    SeekFrom::End(d) => h.len as i128 + d as i128,
                };
                if np < 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
                }
                h.pos = np as u64;
                Ok(h.pos)
            }
        }
    }
}

impl Http {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        // a short forward gap is cheaper to read through than to reconnect
        if let Some((rd, at)) = self.body.as_mut() {
            if *at < self.pos && self.pos - *at <= 512 * 1024 {
                let mut gap = self.pos - *at;
                let mut tmp = [0u8; 16 * 1024];
                while gap > 0 {
                    let want = gap.min(tmp.len() as u64) as usize;
                    let n = rd.read(&mut tmp[..want])?;
                    if n == 0 {
                        break;
                    }
                    gap -= n as u64;
                    *at += n as u64;
                }
            }
        }
        if self.body.as_ref().map_or(true, |(_, at)| *at != self.pos) {
            let end = (self.pos + 64 * 1024 * 1024).min(self.len) - 1;
            let resp = self
                .agent
                .get(&self.url)
                .set("Range", &format!("bytes={}-{}", self.pos, end))
                .call()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            if resp.status() != 206 {
                return Err(io::Error::new(io::ErrorKind::Other, "range not honoured"));
            }
            self.body = Some((Box::new(resp.into_reader()), self.pos));
        }
        let (rd, at) = self.body.as_mut().unwrap();
        let n = rd.read(buf)?;
        *at += n as u64;
        self.pos += n as u64;
        if n == 0 {
            self.body = None; // the 64 MB window ended: next read reopens at pos
            if self.pos < self.len {
                return self.read(buf);
            }
        }
        Ok(n)
    }
}
