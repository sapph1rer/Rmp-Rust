//! ISO 639 language codes -> readable names (Matroska uses 639-2 like "jpn", or BCP47 like "pt-BR").

const NAMES: &[(&str, &str, &str)] = &[
    ("tha", "th", "Thai"),
    ("eng", "en", "English"),
    ("jpn", "ja", "Japanese"),
    ("kor", "ko", "Korean"),
    ("chi", "zh", "Chinese"),
    ("zho", "zh", "Chinese"),
    ("fre", "fr", "French"),
    ("fra", "fr", "French"),
    ("ger", "de", "German"),
    ("deu", "de", "German"),
    ("spa", "es", "Spanish"),
    ("por", "pt", "Portuguese"),
    ("ita", "it", "Italian"),
    ("rus", "ru", "Russian"),
    ("ara", "ar", "Arabic"),
    ("hin", "hi", "Hindi"),
    ("ind", "id", "Indonesian"),
    ("vie", "vi", "Vietnamese"),
    ("msa", "ms", "Malay"),
    ("may", "ms", "Malay"),
    ("fil", "tl", "Filipino"),
    ("tgl", "tl", "Tagalog"),
    ("dut", "nl", "Dutch"),
    ("nld", "nl", "Dutch"),
    ("dan", "da", "Danish"),
    ("fin", "fi", "Finnish"),
    ("nor", "no", "Norwegian"),
    ("nob", "nb", "Norwegian"),
    ("swe", "sv", "Swedish"),
    ("pol", "pl", "Polish"),
    ("cze", "cs", "Czech"),
    ("ces", "cs", "Czech"),
    ("hun", "hu", "Hungarian"),
    ("rum", "ro", "Romanian"),
    ("ron", "ro", "Romanian"),
    ("bul", "bg", "Bulgarian"),
    ("gre", "el", "Greek"),
    ("ell", "el", "Greek"),
    ("heb", "he", "Hebrew"),
    ("tur", "tr", "Turkish"),
    ("ukr", "uk", "Ukrainian"),
    ("hrv", "hr", "Croatian"),
    ("srp", "sr", "Serbian"),
    ("slv", "sl", "Slovenian"),
    ("slo", "sk", "Slovak"),
    ("slk", "sk", "Slovak"),
    ("cat", "ca", "Catalan"),
    ("ice", "is", "Icelandic"),
    ("isl", "is", "Icelandic"),
    ("per", "fa", "Persian"),
    ("fas", "fa", "Persian"),
    ("ben", "bn", "Bengali"),
    ("tam", "ta", "Tamil"),
    ("tel", "te", "Telugu"),
    ("urd", "ur", "Urdu"),
    ("lat", "la", "Latin"),
    ("lav", "lv", "Latvian"),
    ("lit", "lt", "Lithuanian"),
    ("est", "et", "Estonian"),
    ("khm", "km", "Khmer"),
    ("lao", "lo", "Lao"),
    ("bur", "my", "Burmese"),
    ("mya", "my", "Burmese"),
];

/// "jpn" / "ja" / "pt-BR" -> "Japanese" / "Japanese" / "Portuguese". Unknown codes are returned as-is;
/// empty / "und" gives an empty string.
pub fn name(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    let base = c.split(['-', '_']).next().unwrap_or("");
    if base.is_empty() || base == "und" || base == "mis" || base == "zxx" {
        return String::new();
    }
    for (a, b, n) in NAMES {
        if base == *a || base == *b {
            return n.to_string();
        }
    }
    code.trim().to_string()
}

/// Sort priority of a language for menus (lower = earlier): Thai, English, everything else.
pub fn priority(code: &str) -> u8 {
    match name(code).as_str() {
        "Thai" => 0,
        "English" => 1,
        "" => 9,
        _ => 5,
    }
}

/// Index of the first entry of `prefs` ("Thai", "jpn", "en" ... comma separated list already split)
/// that matches the Matroska language `code`.
pub fn pref_rank(code: &str, prefs: &[String]) -> Option<usize> {
    let n = name(code);
    if n.is_empty() {
        return None;
    }
    prefs.iter().position(|p| {
        let p = p.trim();
        !p.is_empty() && (name(p).eq_ignore_ascii_case(&n) || p.eq_ignore_ascii_case(&n))
    })
}

/// "Thai, jpn ,en" -> ["Thai","jpn","en"]
pub fn parse_list(s: &str) -> Vec<String> {
    s.split([',', ';']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Language hint from a file name such as "Movie.th.srt" or "Movie.eng.forced.ass".
pub fn from_filename(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    for part in stem.rsplit('.').take(3) {
        if part.len() >= 2 && part.len() <= 3 {
            let n = name(part);
            if !n.is_empty() && n != part.trim() {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        assert_eq!(name("jpn"), "Japanese");
        assert_eq!(name("pt-BR"), "Portuguese");
        assert_eq!(name("und"), "");
        assert_eq!(from_filename("/a/Obsession (2026) 2160p.th.srt").as_deref(), Some("Thai"));
        assert_eq!(from_filename("movie.srt"), None);
        let prefs = parse_list("Thai, jpn");
        assert_eq!(pref_rank("tha", &prefs), Some(0));
        assert_eq!(pref_rank("ja", &prefs), Some(1));
        assert_eq!(pref_rank("eng", &prefs), None);
    }
}
