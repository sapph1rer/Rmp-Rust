//! `vplayer --encode in out` : re-encode video on the GPU when an encoder exists
//! (NVENC / QSV / AMF / VideoToolbox), otherwise fall back to the software
//! encoder. Audio and subtitles are stream-copied.

use anyhow::{anyhow, bail, Result};
use ffmpeg_next as ff;
use ff::software::scaling::{Context as Scaler, Flags};
use ff::{codec, encoder, format, frame, media, Dictionary, Packet, Rational};

fn candidates(codec: &str, forced: Option<&str>) -> Vec<String> {
    if let Some(f) = forced {
        return vec![f.to_string()];
    }
    let (gpu, sw): (&[&str], &str) = match codec {
        "hevc" | "h265" => (&["hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_videotoolbox"], "libx265"),
        "av1" => (&["av1_nvenc", "av1_qsv", "av1_amf"], "libsvtav1"),
        _ => (&["h264_nvenc", "h264_qsv", "h264_amf", "h264_videotoolbox"], "libx264"),
    };
    let mut v: Vec<String> = gpu.iter().map(|s| s.to_string()).collect();
    v.push(sw.to_string());
    v.push("mpeg4".into()); // last resort, always built in
    v
}

pub struct EncodeOpts {
    pub input: String,
    pub output: String,
    pub codec: String,
    pub encoder: Option<String>,
    pub bitrate: Option<usize>,
    pub cpu_only: bool,
}

pub fn run(o: EncodeOpts) -> Result<()> {
    let mut ictx = format::input(&o.input)?;
    let vin = ictx.streams().best(media::Type::Video).ok_or_else(|| anyhow!("no video stream"))?;
    let vidx = vin.index();
    let in_tb = vin.time_base();
    let fr = vin.avg_frame_rate();
    let fr = if fr.numerator() > 0 { fr } else { Rational::new(30, 1) };
    let mut dec = codec::Context::from_parameters(vin.parameters())?.decoder().video()?;
    let (w, h) = (dec.width(), dec.height());

    let mut octx = format::output(&o.output)?;
    let global = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

    // pick the first encoder that actually opens (GPU ones fail when no device is present)
    let mut enc_opened = None;
    for name in candidates(&o.codec, o.encoder.as_deref()) {
        if o.cpu_only && (name.contains("nvenc") || name.contains("qsv") || name.contains("amf") || name.contains("videotoolbox")) {
            continue;
        }
        let Some(c) = encoder::find_by_name(&name) else { continue };
        let mut e = codec::Context::new_with_codec(c).encoder().video()?;
        e.set_width(w);
        e.set_height(h);
        e.set_format(format::Pixel::YUV420P);
        e.set_time_base(fr.invert());
        e.set_frame_rate(Some(fr));
        e.set_bit_rate(o.bitrate.unwrap_or(((w * h) as usize * 4).min(20_000_000)));
        if global {
            e.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let mut d = Dictionary::new();
        if name.contains("nvenc") { d.set("preset", "p5"); d.set("rc", "vbr"); }
        if name == "libx264" || name == "libx265" { d.set("preset", "medium"); }
        match e.open_with(d) {
            Ok(opened) => {
                eprintln!("encoder: {name}{}", if name.contains("nvenc") || name.contains("qsv") || name.contains("amf") || name.contains("videotoolbox") { " (GPU)" } else { "" });
                enc_opened = Some(opened);
                break;
            }
            Err(err) => eprintln!("encoder {name} unavailable: {err}"),
        }
    }
    let mut enc = enc_opened.ok_or_else(|| anyhow!("no usable video encoder"))?;

    let mut vout = octx.add_stream(encoder::find(enc.id()))?;
    vout.set_parameters(&enc);
    let vout_idx = vout.index();
    // stream-copy audio + subtitles
    let mut map = std::collections::HashMap::new();
    for s in ictx.streams() {
        let t = s.parameters().medium();
        let keep_subs = o.output.to_lowercase().ends_with(".mkv");
        if t == media::Type::Audio || (t == media::Type::Subtitle && keep_subs) {
            let mut so = octx.add_stream(encoder::find(codec::Id::None))?;
            so.set_parameters(s.parameters());
            unsafe { (*so.parameters().as_mut_ptr()).codec_tag = 0 };
            map.insert(s.index(), (so.index(), s.time_base()));
        }
    }
    octx.write_header()?;
    let out_tbs: Vec<Rational> = octx.streams().map(|s| s.time_base()).collect();
    let enc_tb = enc.time_base();

    let mut scaler: Option<Scaler> = None;
    let mut n: i64 = 0;
    let mut write = |octx: &mut format::context::Output, enc: &mut encoder::Video, f: Option<&frame::Video>| -> Result<()> {
        match f { Some(f) => enc.send_frame(f)?, None => enc.send_eof()? }
        let mut p = Packet::empty();
        while enc.receive_packet(&mut p).is_ok() {
            p.set_stream(vout_idx);
            p.rescale_ts(enc_tb, out_tbs[vout_idx]);
            p.write_interleaved(octx)?;
        }
        Ok(())
    };

    let mut df = frame::Video::empty();
    for (stream, mut pkt) in ictx.packets() {
        if stream.index() == vidx {
            if dec.send_packet(&pkt).is_err() { continue; }
            while dec.receive_frame(&mut df).is_ok() {
                let sc = scaler.get_or_insert_with(|| Scaler::get(df.format(), w, h, format::Pixel::YUV420P, w, h, Flags::BILINEAR).expect("scaler"));
                let mut out = frame::Video::empty();
                sc.run(&df, &mut out)?;
                out.set_pts(Some(n));
                n += 1;
                write(&mut octx, &mut enc, Some(&out))?;
                if n % 300 == 0 { eprint!("\rframes: {n}"); }
            }
        } else if let Some((oi, itb)) = map.get(&stream.index()) {
            pkt.rescale_ts(*itb, out_tbs[*oi]);
            pkt.set_stream(*oi);
            pkt.set_position(-1);
            pkt.write_interleaved(&mut octx)?;
        }
    }
    let _ = in_tb;
    dec.send_eof().ok();
    while dec.receive_frame(&mut df).is_ok() {
        let sc = scaler.get_or_insert_with(|| Scaler::get(df.format(), w, h, format::Pixel::YUV420P, w, h, Flags::BILINEAR).expect("scaler"));
        let mut out = frame::Video::empty();
        sc.run(&df, &mut out)?;
        out.set_pts(Some(n));
        n += 1;
        write(&mut octx, &mut enc, Some(&out))?;
    }
    write(&mut octx, &mut enc, None)?;
    octx.write_trailer()?;
    if n == 0 { bail!("no frames were encoded"); }
    eprintln!("\ndone: {n} frames -> {}", o.output);
    Ok(())
}
