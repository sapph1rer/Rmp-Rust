//! Finding fonts that can show Thai (and Latin) text: used for the UI and for subtitles.

use crate::subrender::FontData;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Choice {
    pub name: &'static str,
    pub regular: &'static [&'static str],
    pub bold: &'static [&'static str],
}

/// Thai-capable families, best looking for subtitles first.
pub const CHOICES: &[Choice] = &[
    Choice { name: "Leelawadee UI", regular: &["LeelawUI.ttf", "leelawui.ttf"], bold: &["LeelaUIb.ttf", "leelaUIb.ttf"] },
    Choice { name: "Noto Sans Thai", regular: &["NotoSansThai-Regular.ttf", "NotoSansThai[wdth,wght].ttf", "NotoSansThai-VariableFont_wdth,wght.ttf"], bold: &["NotoSansThai-Bold.ttf"] },
    Choice { name: "Sarabun", regular: &["Sarabun-Regular.ttf"], bold: &["Sarabun-Bold.ttf"] },
    Choice { name: "Kanit", regular: &["Kanit-Regular.ttf"], bold: &["Kanit-SemiBold.ttf", "Kanit-Bold.ttf"] },
    Choice { name: "Prompt", regular: &["Prompt-Regular.ttf"], bold: &["Prompt-SemiBold.ttf", "Prompt-Bold.ttf"] },
    Choice { name: "Tahoma", regular: &["tahoma.ttf"], bold: &["tahomabd.ttf"] },
    Choice { name: "Microsoft Sans Serif", regular: &["micross.ttf"], bold: &[] },
    Choice { name: "Angsana New", regular: &["angsa.ttf"], bold: &["angsab.ttf"] },
    Choice { name: "Cordia New", regular: &["cordia.ttf"], bold: &["cordiab.ttf"] },
    Choice { name: "Ayuthaya", regular: &["Ayuthaya.ttf"], bold: &[] },
    Choice { name: "Thonburi", regular: &["Thonburi.ttc"], bold: &[] },
    Choice { name: "Loma", regular: &["Loma.otf", "Loma.ttf"], bold: &["Loma-Bold.otf", "Loma-Bold.ttf"] },
    Choice { name: "Garuda", regular: &["Garuda.otf", "Garuda.ttf"], bold: &["Garuda-Bold.otf", "Garuda-Bold.ttf"] },
    Choice { name: "Noto Sans CJK / FreeSans", regular: &["FreeSans.ttf", "NotoSansCJK-Regular.ttc"], bold: &["FreeSansBold.ttf", "NotoSansCJK-Bold.ttc"] },
];

fn dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(w) = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")) {
        v.push(PathBuf::from(w).join("Fonts"));
    }
    v.push(PathBuf::from(r"C:\Windows\Fonts"));
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        v.push(PathBuf::from(l).join(r"Microsoft\Windows\Fonts"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        let h = PathBuf::from(h);
        v.push(h.join(".fonts"));
        v.push(h.join(".local/share/fonts"));
        v.push(h.join("Library/Fonts"));
    }
    for d in [
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
        "/Library/Fonts",
        "/usr/share/fonts/opentype/tlwg",
        "/usr/share/fonts/truetype/tlwg",
        "/usr/share/fonts/opentype/noto",
        "/usr/share/fonts/truetype/noto",
        "/usr/share/fonts/noto",
        "/usr/share/fonts/truetype/freefont",
    ] {
        v.push(PathBuf::from(d));
    }
    v
}

fn find(names: &[&str]) -> Option<PathBuf> {
    let ds = dirs();
    for n in names {
        for d in &ds {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Names of the fonts of `CHOICES` that exist on this machine.
pub fn available() -> Vec<&'static str> {
    CHOICES.iter().filter(|c| find(c.regular).is_some()).map(|c| c.name).collect()
}

pub fn load_file(path: &std::path::Path) -> Option<FontData> {
    let bytes = std::fs::read(path).ok()?;
    Some(FontData { bytes: Arc::new(bytes), index: 0 })
}

/// Resolve the `sub_font` setting ("" = automatic, a name from `CHOICES`, or a file path).
pub fn resolve(setting: &str, bold: bool) -> Option<(String, FontData)> {
    let setting = setting.trim();
    if !setting.is_empty() {
        if let Some(c) = CHOICES.iter().find(|c| c.name.eq_ignore_ascii_case(setting)) {
            if let Some(f) = pick(c, bold) {
                return Some(f);
            }
        } else if let Some(f) = load_file(std::path::Path::new(setting)) {
            return Some((setting.to_string(), f));
        }
    }
    CHOICES.iter().find_map(|c| pick(c, bold))
}

fn pick(c: &Choice, bold: bool) -> Option<(String, FontData)> {
    let path = if bold { find(c.bold).or_else(|| find(c.regular)) } else { find(c.regular) }?;
    Some((c.name.to_string(), load_file(&path)?))
}
