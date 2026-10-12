[readme_md.md](https://github.com/user-attachments/files/33344205/readme_md.md)
# Rmp-Rust

A media player and processing backend written in Rust. This repository contains modular components for handling audio, video codecs, MKV containers, subtitles, hardware acceleration, network streaming, and player control.

---

## 📂 Project Structure

Here is a quick look at how the codebase is organized under the `src/` directory:

*   **Core Application & Main Entry:**
    *   `main.rs` – The entry point of the application.
    *   `app.rs` – Core application logic and state management.
    *   `config.rs` – Configuration handling and settings.
    *   `player.rs` – Main media player control logic.
    *   `report.rs` – Logging, diagnostics, and reporting.
*   **Backend & Codecs:**
    *   `backend/` – Core system backends (including `mf.rs` for Media Foundation integration and module controls).
    *   `codecs.rs` & `vcodecs.rs` – Video and media codec handling.
    *   `swvideo.rs` – Software video rendering and processing.
    *   `transcode.rs` – Transcoding pipeline.
*   **Media Formats & Subtitles:**
    *   `mkv.rs` & `mkvaudio.rs` – MKV container parsing and audio handling.
    *   `subs.rs` & `subrender.rs` – Subtitle parsing and text/graphic rendering.
*   **Audio & Hardware:**
    *   `extaudio.rs` – External audio handling and outputs.
    *   `dolby.rs` – Dolby audio processing/support.
    *   `hw.rs` – Hardware acceleration interfaces.
*   **Utilities & System:**
    *   `net.rs` – Networking and stream handling.
    *   `fonts.rs` – Font loading and management for text/subtitles.
    *   `lang.rs` – Localization and language support.
    *   `log.rs` – Logging system.
    *   `srcio.rs` – Source input/output operations.

---

## 🚀 Getting Started

Make sure you have [Rust](https://www.rust-lang.org/) installed on your machine.

1. **Clone the repository:**
   ```bash
   git clone https://github.com/sapph1rer/Rmp-Rust.git
   cd Rmp-Rust
   ```

2. **Build the project:**
   ```bash
   cargo build --release
   ```

3. **Run the application:**
   ```bash
   cargo run
   ```

---

## 🤝 Contributing

Contributions, issues, and feature requests are welcome! Feel free to check out the [issues page](https://github.com/sapph1rer/Rmp-Rust/issues).

---

## 📝 License

This project is open-source. Please check the repository for license details.
