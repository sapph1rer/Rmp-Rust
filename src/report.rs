//! "Report problem": posts the user's message plus a diagnostics file to a Discord webhook.
//!
//! The webhook address is compiled in (override with the RMP_REPORT_WEBHOOK environment variable).
//! Anyone who owns the binary can read it and post to that channel, so if Rmp is distributed
//! publicly, rotate the webhook and put a small relay in front of it instead.

use std::time::{SystemTime, UNIX_EPOCH};

pub const WEBHOOK: &str = "https://discord.com/api/webhooks/1474349984039043185/uFDeAZ-PkVL0VyOPYNcOa7ZT7A3GoYwNkkUeJjR7GIntMbLqzOF3yKLHdyRqUaIj7n_w";

pub fn webhook_url() -> String {
    std::env::var("RMP_REPORT_WEBHOOK").ok().filter(|s| s.starts_with("https://") || s.starts_with("http://")).unwrap_or_else(|| WEBHOOK.to_string())
}

/// UTC time as ISO 8601 (Discord embed timestamp), no external crate.
pub fn iso_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Windows version / CPU line for the report.
pub fn system_summary() -> String {
    let mut s = format!("Rmp {} | {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    if let Ok(n) = std::env::var("NUMBER_OF_PROCESSORS") {
        s.push_str(&format!(" | {n} logical CPUs"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Ok(o) = std::process::Command::new("cmd").args(["/C", "ver"]).creation_flags(0x0800_0000).output() {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !v.is_empty() {
                s.push_str(&format!(" | {v}"));
            }
        }
    }
    s
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// multipart/form-data body: `payload_json` + one attached text file.
pub fn build_body(boundary: &str, message: &str, system: &str, file_name: &str, file_text: &str) -> Vec<u8> {
    let payload = format!(
        "{{\"username\":\"Rmp reports\",\"allowed_mentions\":{{\"parse\":[]}},\"embeds\":[{{\"title\":\"Problem report\",\"description\":{},\"color\":15158332,\"fields\":[{{\"name\":\"System\",\"value\":{}}}],\"timestamp\":{}}}]}}",
        json_str(&truncate(if message.trim().is_empty() { "(no description)" } else { message.trim() }, 3800)),
        json_str(&truncate(system, 1000)),
        json_str(&iso_now())
    );
    let mut b = Vec::new();
    b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{payload}\r\n").as_bytes());
    b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files[0]\"; filename=\"{file_name}\"\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n").as_bytes());
    b.extend_from_slice(file_text.as_bytes());
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

pub fn send(url: &str, message: &str, system: &str, attachment: &str) -> Result<(), String> {
    let boundary = format!("rmp{:x}{:x}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0), std::process::id());
    // Discord accepts ~8-25 MB per message; keep the attachment small
    let bytes = attachment.as_bytes();
    let attach = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(1_500_000)..]).into_owned();
    let body = build_body(&boundary, message, system, "rmp-report.txt", &attach);
    let r = crate::srcio::agent().post(url).set("Content-Type", &format!("multipart/form-data; boundary={boundary}")).send_bytes(&body);
    match r {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(429, _)) => Err("Too many reports right now, try again in a minute.".into()),
        Err(ureq::Error::Status(c, _)) => Err(format!("The report server answered HTTP {c}.")),
        Err(e) => Err(format!("Could not send: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn iso_time_shape() {
        let t = iso_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }

    #[test]
    fn body_is_safe_and_posted() {
        // message with quotes / mentions must stay inside valid JSON and not ping anyone
        let b = build_body("BND", "he said \"hi\" @everyone\nline2", "sys", "r.txt", "log text");
        let s = String::from_utf8(b).unwrap();
        assert!(s.contains("\"allowed_mentions\":{\"parse\":[]}"));
        assert!(s.contains("he said \\\"hi\\\" @everyone\\nline2"));
        assert!(s.ends_with("--BND--\r\n"));

        // a local server stands in for Discord
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            loop {
                let n = c.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let t = String::from_utf8_lossy(&got).into_owned();
                if t.contains("--\r\n") && t.contains("log text") {
                    break;
                }
            }
            c.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });
        send(&format!("http://127.0.0.1:{port}/api/webhooks/1/x"), "problem", "sys", "log text").unwrap();
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /api/webhooks/1/x"));
        assert!(req.contains("multipart/form-data; boundary="));
        assert!(req.contains("filename=\"rmp-report.txt\""));
    }
}
