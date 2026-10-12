//! Tiny file logger: release builds have no console (windows subsystem), so errors go to
//! `%LOCALAPPDATA%\Rmp\rmp.log` (or the temp dir). Also echoed to stderr.
use std::io::Write;
use std::sync::OnceLock;

pub fn path() -> &'static std::path::PathBuf {
    static P: OnceLock<std::path::PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("XDG_STATE_HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join("Rmp");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("rmp.log");
        if std::fs::metadata(&p).map(|m| m.len() > 1_000_000).unwrap_or(false) {
            let _ = std::fs::remove_file(&p);
        }
        p
    })
}

pub fn write(msg: &str) {
    eprintln!("rmp: {msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{t}] {msg}");
    }
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        write(&format!("PANIC: {info}"));
    }));
}

/// Last `max` bytes of the log file (for problem reports).
pub fn tail(max: usize) -> String {
    let Ok(b) = std::fs::read(path()) else { return String::new() };
    let start = b.len().saturating_sub(max);
    String::from_utf8_lossy(&b[start..]).into_owned()
}
