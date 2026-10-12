//! Subtitle text renderer with real text shaping (rustybuzz = HarfBuzz in pure Rust), so Thai tone
//! marks / upper vowels are stacked correctly, then rasterised with ab_glyph and drawn with an
//! outline, shadow and optional background box into one RGBA bitmap per cue.

use ab_glyph::{Font, FontRef, GlyphId, PxScale};
use std::sync::Arc;

pub struct FontData {
    pub bytes: Arc<Vec<u8>>,
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub size: f32,       // em size in pixels
    pub outline: f32,    // px
    pub shadow: f32,     // px offset (0 = none)
    pub bg_alpha: u8,    // 0 = no box
    pub color: [u8; 3],
    pub max_width: f32,  // px, lines wrap beyond this
    pub line_gap: f32,   // extra line spacing as a fraction of size
}

pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    /// premultiplied RGBA
    pub rgba: Vec<u8>,
}

struct Run {
    glyphs: Vec<(GlyphId, f32, f32)>, // id, x, y_offset (px, y up)
    width: f32,
}

pub struct Renderer {
    data: FontData,
}

impl Renderer {
    pub fn new(data: FontData) -> Option<Renderer> {
        FontRef::try_from_slice_and_index(&data.bytes, data.index).ok()?;
        rustybuzz::Face::from_slice(&data.bytes, data.index)?;
        Some(Renderer { data })
    }

    /// True when the font has a glyph for every visible character of `text`.
    pub fn covers(&self, text: &str) -> bool {
        let Ok(font) = FontRef::try_from_slice_and_index(&self.data.bytes, self.data.index) else { return false };
        text.chars().all(|c| c.is_whitespace() || c.is_control() || font.glyph_id(c).0 != 0)
    }

    fn shape(&self, face: &rustybuzz::Face, text: &str, px_per_unit: f32) -> Run {
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        let out = rustybuzz::shape(face, &[], buf);
        let (infos, pos) = (out.glyph_infos(), out.glyph_positions());
        let mut x = 0.0f32;
        let mut glyphs = Vec::with_capacity(infos.len());
        for (i, p) in infos.iter().zip(pos) {
            glyphs.push((GlyphId(i.glyph_id as u16), x + p.x_offset as f32 * px_per_unit, p.y_offset as f32 * px_per_unit));
            x += p.x_advance as f32 * px_per_unit;
        }
        Run { glyphs, width: x }
    }

    /// Break one paragraph into lines no wider than `max_w` (at spaces when possible, otherwise at a
    /// character boundary — Thai has no spaces).
    fn wrap(&self, face: &rustybuzz::Face, line: &str, ppu: f32, max_w: f32) -> Vec<String> {
        let whole = self.shape(face, line, ppu);
        if whole.width <= max_w {
            return vec![line.to_string()];
        }
        let chars: Vec<char> = line.chars().collect();
        let mut out = Vec::new();
        let mut start = 0;
        while start < chars.len() {
            // largest prefix that fits (binary search on the shaped width)
            let (mut lo, mut hi) = (start + 1, chars.len());
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                let s: String = chars[start..mid].iter().collect();
                if self.shape(face, &s, ppu).width <= max_w {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            let mut end = lo;
            if end < chars.len() {
                // prefer a space; never split before a combining mark / Thai vowel-above-tone
                if let Some(sp) = (start + 1..end).rev().find(|&i| chars[i] == ' ') {
                    end = sp + 1;
                } else {
                    while end > start + 1 && is_combining(chars[end]) {
                        end -= 1;
                    }
                    // Thai leading vowels (เ แ โ ใ ไ) belong to the next consonant
                    while end > start + 1 && matches!(chars[end - 1], '\u{0E40}'..='\u{0E44}') {
                        end -= 1;
                    }
                }
            }
            out.push(chars[start..end].iter().collect::<String>().trim_end().to_string());
            start = end;
        }
        out
    }

    pub fn render(&self, text: &str, st: &Style) -> Option<Bitmap> {
        let face = rustybuzz::Face::from_slice(&self.data.bytes, self.data.index)?;
        let font = FontRef::try_from_slice_and_index(&self.data.bytes, self.data.index).ok()?;
        let upem = face.units_per_em() as f32;
        let ppu = st.size / upem; // px per font unit
        let scale = PxScale::from(st.size * font.height_unscaled() / font.units_per_em().unwrap_or(upem));
        let (asc, desc) = (face.ascender() as f32 * ppu, -(face.descender() as f32) * ppu);
        let line_h = (asc + desc) * (1.0 + st.line_gap);

        let mut lines: Vec<Run> = Vec::new();
        for para in text.lines() {
            let para = para.trim_end();
            if para.is_empty() {
                continue;
            }
            for l in self.wrap(&face, para, ppu, st.max_width) {
                lines.push(self.shape(&face, &l, ppu));
            }
        }
        if lines.is_empty() {
            return None;
        }
        let pad = (st.outline.ceil() + st.shadow.ceil() + 2.0).max(3.0);
        let box_pad = if st.bg_alpha > 0 { (st.size * 0.18).ceil() } else { 0.0 };
        let text_w = lines.iter().fold(0.0f32, |m, l| m.max(l.width));
        let w = (text_w + 2.0 * (pad + box_pad)).ceil() as usize;
        let h = (line_h * lines.len() as f32 + 2.0 * (pad + box_pad) + st.size * 0.1).ceil() as usize;
        if w == 0 || h == 0 || w > 8192 || h > 4096 {
            return None;
        }

        // glyph coverage
        let mut cov = vec![0.0f32; w * h];
        for (li, run) in lines.iter().enumerate() {
            let base_y = pad + box_pad + li as f32 * line_h + asc;
            let x0 = pad + box_pad + (text_w - run.width) / 2.0;
            for &(gid, gx, goy) in &run.glyphs {
                let g = gid.with_scale_and_position(scale, ab_glyph::point(x0 + gx, base_y - goy));
                if let Some(o) = font.outline_glyph(g) {
                    let b = o.px_bounds();
                    o.draw(|x, y, c| {
                        let (px, py) = (b.min.x as i32 + x as i32, b.min.y as i32 + y as i32);
                        if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                            let i = py as usize * w + px as usize;
                            cov[i] = (cov[i] + c).min(1.0);
                        }
                    });
                }
            }
        }

        // outline = max coverage inside a disc (separable approximation: two box-max passes + disc test)
        let r = st.outline.max(0.0);
        let outline = if r > 0.0 { dilate(&cov, w, h, r) } else { vec![0.0; w * h] };
        let sh = st.shadow.round() as i32;

        let mut rgba = vec![0u8; w * h * 4];
        let put = |rgba: &mut [u8], i: usize, col: [u8; 3], a: f32| {
            // source-over, premultiplied
            let a = a.clamp(0.0, 1.0);
            let dst_a = rgba[i * 4 + 3] as f32 / 255.0;
            let out_a = a + dst_a * (1.0 - a);
            for k in 0..3 {
                let d = rgba[i * 4 + k] as f32;
                rgba[i * 4 + k] = (col[k] as f32 * a + d * (1.0 - a)).round().min(255.0) as u8;
            }
            rgba[i * 4 + 3] = (out_a * 255.0).round().min(255.0) as u8;
        };
        if st.bg_alpha > 0 {
            let a = st.bg_alpha as f32 / 255.0;
            for i in 0..w * h {
                put(&mut rgba, i, [0, 0, 0], a);
            }
        }
        if sh != 0 {
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let (sx, sy) = (x - sh, y - sh);
                    if sx >= 0 && sy >= 0 && (sx as usize) < w && (sy as usize) < h {
                        let a = outline[sy as usize * w + sx as usize].max(cov[sy as usize * w + sx as usize]) * 0.6;
                        if a > 0.0 {
                            put(&mut rgba, y as usize * w + x as usize, [0, 0, 0], a);
                        }
                    }
                }
            }
        }
        for i in 0..w * h {
            if outline[i] > 0.0 {
                put(&mut rgba, i, [0, 0, 0], outline[i]);
            }
        }
        for i in 0..w * h {
            if cov[i] > 0.0 {
                put(&mut rgba, i, st.color, cov[i]);
            }
        }
        Some(Bitmap { w, h, rgba })
    }
}

fn is_combining(c: char) -> bool {
    matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}' | '\u{0300}'..='\u{036F}')
}

/// Grow the coverage map by radius `r` (anti-aliased disc).
fn dilate(cov: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    let ri = r.ceil() as i32;
    let mut offs: Vec<(i32, i32, f32)> = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d <= r + 0.5 {
                offs.push((dx, dy, (r + 0.5 - d).clamp(0.0, 1.0)));
            }
        }
    }
    let mut out = vec![0.0f32; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut m = 0.0f32;
            for &(dx, dy, k) in &offs {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && (sx as usize) < w && (sy as usize) < h {
                    let c = cov[sy as usize * w + sx as usize] * k;
                    if c > m {
                        m = c;
                        if m >= 1.0 {
                            break;
                        }
                    }
                }
            }
            out[y as usize * w + x as usize] = m;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(path: &str) -> Option<Renderer> {
        let b = std::fs::read(path).ok()?;
        Renderer::new(FontData { bytes: Arc::new(b), index: 0 })
    }

    #[test]
    fn renders_thai_with_marks() {
        let Some(r) = load("/usr/share/fonts/opentype/tlwg/Loma-Bold.otf") else { return };
        let st = Style { size: 48.0, outline: 3.0, shadow: 2.0, bg_alpha: 0, color: [255, 255, 255], max_width: 900.0, line_gap: 0.05 };
        let text = "ผู้ชายที่ปั้นน้ำเป็นตัว\nHello, ก็ได้ครับ ปู่ย่าตายาย";
        assert!(r.covers(text));
        let bm = r.render(text, &st).unwrap();
        assert!(bm.w > 200 && bm.h > 80);
        assert!(bm.rgba.chunks(4).filter(|p| p[3] > 0).count() > 2000);
        // dump for eyeballing
        let ppm: Vec<u8> = bm.rgba.chunks(4).flat_map(|p| {
            let a = p[3] as f32 / 255.0;
            [(p[0] as f32 + 90.0 * (1.0 - a)) as u8, (p[1] as f32 + 90.0 * (1.0 - a)) as u8, (p[2] as f32 + 90.0 * (1.0 - a)) as u8]
        }).collect();
        let mut f = format!("P6\n{} {}\n255\n", bm.w, bm.h).into_bytes();
        f.extend(ppm);
        let _ = std::fs::write("/tmp/claude-0/sub_thai.ppm", f);
    }

    #[test]
    fn wraps_long_thai_without_spaces() {
        let Some(r) = load("/usr/share/fonts/opentype/tlwg/Loma.otf") else { return };
        let st = Style { size: 40.0, outline: 2.0, shadow: 0.0, bg_alpha: 0, color: [255; 3], max_width: 400.0, line_gap: 0.0 };
        let text = "ฉันไม่เคยคิดเลยว่าวันหนึ่งเราจะได้มาพบกันอีกครั้งที่นี่หลังจากผ่านไปนานแสนนาน";
        let one = r.render(text, &Style { max_width: 5000.0, ..st.clone() }).unwrap();
        let wrapped = r.render(text, &st).unwrap();
        assert!(wrapped.w < one.w / 2 && wrapped.h > one.h * 2 - 10, "{}x{} vs {}x{}", wrapped.w, wrapped.h, one.w, one.h);
    }
}
