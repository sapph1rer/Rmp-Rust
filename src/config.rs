//! Persistent settings (JSON) + mpv-style command line parsing.
//! Unknown `--options` are ignored on purpose so a launcher written for mpv
//! (like waku) can call this binary without changes.

use crate::player::Options;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub version: u32, // settings layout / defaults version (for migrations)
    // general
    pub hwdec: String, // auto | no
    pub volume: f32,   // 0..130
    pub muted: bool,
    pub seek_step: f64, // seconds for the skip buttons / arrow keys
    pub volume_step: f32,
    pub autohide_secs: f32,
    pub remember_pos: bool,
    // video
    pub max_height: u32, // cap on the picture height handed to the UI (0 = native)
    // audio
    pub audio_langs: String, // preferred languages, comma separated ("Thai, Japanese")
    pub audio_delay: f64,
    pub mix_center: f32,   // dialogue level in the stereo downmix
    pub mix_surround: f32, // surround level
    pub mix_lfe: f32,      // bass channel level (0 = off)
    // subtitles
    pub sub_langs: String,
    pub sub_font: String, // "" = automatic (best Thai-capable font found), a font name from the list, or a file path
    pub sub_bold: bool,
    pub sub_scale: f32,   // text height in % of the picture height
    pub sub_outline: f32, // px at 1080p
    pub sub_shadow: f32,  // px at 1080p
    pub sub_bg: u8,       // background box opacity 0..100 (%)
    pub sub_margin: f32,  // distance from the bottom in % of the picture height
    pub sub_color: [u8; 3],
    pub sub_delay: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: 2,
            hwdec: "auto".into(),
            volume: 100.0,
            muted: false,
            seek_step: 10.0,
            volume_step: 5.0,
            autohide_secs: 2.5,
            remember_pos: false,
            max_height: 0,
            audio_langs: String::new(),
            audio_delay: 0.0,
            mix_center: 1.0,
            mix_surround: 1.0,
            mix_lfe: 0.0,
            sub_langs: "Thai, English".into(),
            sub_font: String::new(),
            sub_bold: true,
            sub_scale: 4.6,
            sub_outline: 2.4,
            sub_shadow: 1.5,
            sub_bg: 0,
            sub_margin: 5.0,
            sub_color: [255, 255, 255],
            sub_delay: 0.0,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(dir.join("rmp").join("settings.json"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s: Settings = config_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if s.version < 2 {
            s.max_height = 0; // v2: full resolution by default (v1 capped the picture at 1080p)
            s.version = 2;
        }
        s
    }
    pub fn save(&self) {
        if let Some(p) = config_path() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, s);
            }
        }
    }
}

#[derive(Default, Debug)]
pub struct Cli {
    pub target: Option<String>,
    pub opts: Options,
    pub volume: Option<f32>,
    pub mute: bool,
    pub pause: bool,
    pub fullscreen: bool,
    pub headless_secs: Option<f64>,
    pub serve_only: bool,
    pub keep_open: bool,
    pub verbose: bool,
    pub help: bool,
    /// other renditions of the same video: (label such as "720p", url) - for the quality menu
    pub alt_urls: Vec<(String, String)>,
    /// where to write position / duration / ended when closing (read by waku)
    pub position_file: Option<String>,
}

pub const HELP: &str = "rmp - small native video player (Windows Media Foundation, GPU decode)

USAGE: rmp [options] <file | url>

  --hwdec=auto|no              GPU decoding via D3D11/DXVA (default auto)
  --referrer=URL  --user-agent=STR  --http-header-fields=H1,H2
  --force-media-title=STR      --sub-file=PATH (repeatable)
  --keep-open                  stay open at the end (default: exit like mpv when launched with a file)
  --alt-url=LABEL|URL          another quality of the same video (repeatable): adds a Quality menu
  --position-file=PATH         on close write pos=, duration=, ended=, quality= there
  --start=SECONDS              --volume=0..130   --mute   --pause   --fs
  --demuxer-lavf-format=hls    --demuxer-lavf-o=key=value,key=value
  --test-play=SECONDS          headless self-test (no window)

Other mpv options are accepted and ignored.
Keys: Space pause | Left/Right -/+ step | Up/Down volume | M mute | F fullscreen
      S next subtitle | Z/X sub delay | V sub on/off | Ctrl+O open | Ctrl+L open URL | Esc leave fullscreen";

fn val<'a>(args: &'a [String], i: &mut usize, a: &'a str, name: &str) -> Option<String> {
    // supports `--name=value` and `--name value`
    if let Some(v) = a.strip_prefix(&format!("--{name}=")) {
        return Some(v.to_string());
    }
    if a == format!("--{name}") {
        *i += 1;
        return args.get(*i).cloned();
    }
    None
}

/// `720p|https://host/x-720.txt` -> ("720p", url)
pub fn parse_alt(s: &str) -> Option<(String, String)> {
    let (l, u) = s.split_once('|')?;
    let (l, u) = (l.trim(), u.trim());
    (!l.is_empty() && u.starts_with("http")).then(|| (l.to_string(), u.to_string()))
}

pub fn parse(args: Vec<String>) -> Cli {
    let mut c = Cli::default();
    c.opts.hwdec = String::new();
    let mut i = 0;
    let mut only_files = false;
    while i < args.len() {
        let a = args[i].clone();
        i += 1;
        let mut j = i - 1;
        let mut a_ref = a.as_str();
        if only_files || !a_ref.starts_with("--") {
            if c.target.is_none() {
                c.target = Some(a.clone());
            }
            continue;
        }
        if a_ref == "--" {
            only_files = true;
            continue;
        }
        macro_rules! v {
            ($n:expr) => {
                val(&args, &mut j, a_ref, $n)
            };
        }
        if let Some(x) = v!("referrer").or_else(|| v!("referer")) {
            c.opts.referrer = Some(x);
        } else if let Some(x) = v!("user-agent") {
            c.opts.user_agent = Some(x);
        } else if let Some(x) = v!("http-header-fields") {
            c.opts.headers.extend(x.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        } else if let Some(x) = v!("force-media-title") {
            c.opts.title = Some(x);
        } else if let Some(x) = v!("sub-file") {
            c.opts.external_subs.push(x);
        } else if let Some(x) = v!("alt-url") {
            if let Some((l, u)) = parse_alt(&x) {
                c.alt_urls.push((l, u));
            }
        } else if let Some(x) = v!("position-file") {
            c.position_file = Some(x);
        } else if let Some(x) = v!("start") {
            c.opts.start = x.trim_start_matches('+').parse().unwrap_or(0.0);
        } else if let Some(x) = v!("hwdec") {
            c.opts.hwdec = x;
        } else if let Some(x) = v!("volume") {
            c.volume = x.parse().ok();
        } else if let Some(x) = v!("demuxer-lavf-format") {
            c.opts.force_format = Some(x);
        } else if let Some(x) = v!("demuxer-lavf-o") {
            for kv in x.split(',') {
                if let Some((k, vv)) = kv.split_once('=') {
                    c.opts.demuxer_opts.push((k.trim().to_string(), vv.trim().to_string()));
                }
            }
        } else if let Some(x) = v!("test-play") {
            c.headless_secs = x.parse().ok();
        } else if a_ref == "--keep-open" || a_ref == "--keep-open=yes" || a_ref == "--keep-open=always" {
            c.keep_open = true;
        } else if a_ref == "--serve-only" {
            c.serve_only = true;
        } else if a_ref == "--verbose" || a_ref == "-v" {
            c.verbose = true;
        } else if a_ref == "--mute" || a_ref == "--mute=yes" {
            c.mute = true;
        } else if a_ref == "--pause" || a_ref == "--pause=yes" {
            c.pause = true;
        } else if a_ref == "--fs" || a_ref == "--fullscreen" {
            c.fullscreen = true;
        } else if a_ref == "--help" || a_ref == "-h" {
            c.help = true;
        }
        // `--opt value` forms consumed above advanced j; keep `i` in sync
        if j + 1 > i {
            i = j + 1;
        }
        a_ref = "";
        let _ = a_ref;
    }
    c
}

#[cfg(test)]
mod tests_alt {
    use super::*;
    #[test]
    fn alt_and_position_args() {
        let a: Vec<String> = ["--alt-url=1080p|https://h/a-1080.txt", "--alt-url=720p|https://h/a-720.txt?x=1|y", "--alt-url=bad", "--position-file=C:\\t\\p.txt", "--start=30", "https://h/a-720.txt"]
            .iter().map(|s| s.to_string()).collect();
        let c = parse(a);
        assert_eq!(c.alt_urls.len(), 2);
        assert_eq!(c.alt_urls[1], ("720p".into(), "https://h/a-720.txt?x=1|y".into()));
        assert_eq!(c.position_file.as_deref(), Some("C:\\t\\p.txt"));
        assert_eq!(c.opts.start, 30.0);
        assert_eq!(c.target.as_deref(), Some("https://h/a-720.txt"));
    }
}
