//! Hardware (GPU) decoding helpers shared by the player and the transcoder.

use ffmpeg_next as ff;
use ff::ffi;
use std::ffi::{c_int, c_void, CString};
use std::ptr;

#[derive(Clone, Debug)]
pub struct HwInfo {
    pub name: String,
    pub pix_fmt: i32,
}

/// mpv-style `--hwdec` value -> list of FFmpeg device types to try, in order.
pub fn candidates(pref: &str) -> Vec<String> {
    let p = pref.trim().to_lowercase();
    let p = p.trim_end_matches("-copy");
    let v: Vec<&str> = match p {
        "no" | "none" | "off" | "false" | "software" => vec![],
        "" | "auto" | "auto-safe" | "yes" => {
            if cfg!(windows) {
                vec!["d3d11va", "cuda", "qsv", "dxva2"]
            } else if cfg!(target_os = "macos") {
                vec!["videotoolbox"]
            } else {
                vec!["vaapi", "cuda", "vdpau"]
            }
        }
        "nvdec" | "cuda" | "nvidia" => vec!["cuda"],
        other => vec![other],
    };
    v.into_iter().map(String::from).collect()
}

unsafe extern "C" fn get_format(ctx: *mut ffi::AVCodecContext, fmts: *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    let want = (*ctx).opaque as isize as c_int;
    let mut p = fmts;
    let mut last = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p as c_int == want {
            return *p;
        }
        last = *p;
        p = p.add(1);
    }
    last // hw format not offered: fall back to the software format
}

/// Try to attach a GPU device to `ctx` (must be called before the decoder is opened).
///
/// # Safety
/// `ctx` and `codec` must be valid pointers.
pub unsafe fn attach(ctx: *mut ffi::AVCodecContext, codec: *const ffi::AVCodec, pref: &str) -> Option<HwInfo> {
    for name in candidates(pref) {
        let cname = CString::new(name.clone()).ok()?;
        let dtype = ffi::av_hwdevice_find_type_by_name(cname.as_ptr());
        if dtype == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            continue;
        }
        let mut fmt = None;
        let mut i = 0;
        loop {
            let cfg = ffi::avcodec_get_hw_config(codec, i);
            if cfg.is_null() {
                break;
            }
            if (*cfg).methods & (ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int) != 0 && (*cfg).device_type == dtype {
                fmt = Some((*cfg).pix_fmt as c_int);
                break;
            }
            i += 1;
        }
        let Some(fmt) = fmt else { continue };
        let mut dev: *mut ffi::AVBufferRef = ptr::null_mut();
        if ffi::av_hwdevice_ctx_create(&mut dev, dtype, ptr::null(), ptr::null_mut(), 0) < 0 {
            continue;
        }
        (*ctx).hw_device_ctx = dev;
        (*ctx).opaque = fmt as isize as *mut c_void;
        (*ctx).get_format = Some(get_format);
        return Some(HwInfo { name, pix_fmt: fmt });
    }
    None
}

/// If `frame` lives in GPU memory, download it into `sw` (NV12 / P010 / ...).
/// Returns false on failure.
pub fn download(frame: &ff::frame::Video, sw: &mut ff::frame::Video, hw_fmt: i32) -> bool {
    unsafe {
        if (*frame.as_ptr()).format != hw_fmt {
            return false;
        }
        if ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) < 0 {
            return false;
        }
        ffi::av_frame_copy_props(sw.as_mut_ptr(), frame.as_ptr());
        true
    }
}
