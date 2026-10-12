//! Decoding backend. Contract (see player.rs): the backend fills `Shared`:
//! info/has_video/has_audio, pushes `Frame`s + PCM via push_frame/push_audio,
//! reacts to `Cmd::Seek` / `Cmd::SetAudio`, and sets video_eof/audio_eof.

#[cfg(windows)]
mod mf;
#[cfg(windows)]
pub use mf::run;

#[cfg(not(windows))]
pub fn run(
    _sh: std::sync::Arc<crate::player::Shared>,
    _url: String,
    _opts: crate::player::Options,
    _rx: std::sync::mpsc::Receiver<crate::player::Cmd>,
) -> anyhow::Result<()> {
    anyhow::bail!("this build of rmp only decodes on Windows (Media Foundation)")
}
