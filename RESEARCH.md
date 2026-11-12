# playr — research notes

Gathered 2026-11-11 by four research agents for the task "light, fast media player in Rust:
MP4/AVI/etc. video, FLAC/AAC/MP3 audio, internet radio". Versions were read from the crates.io
API on that date. Conformance and speed figures for young crates are **their authors' README
claims, not tested here** unless a line says "verified locally".

## Decisions made by the user

- Prefer Rust and Rust-based libraries wherever possible.
- Young / unproven pure-Rust crates are acceptable where the mature ecosystem has a gap
  (HE-AAC, Opus, and by extension video decoding).
- Sample videos live in `C:\Users\domek\Videos\Captures` (read-only use).

## Local machine

- Windows 11 Pro 10.0.26200, i7-10750H, 32 GB RAM, Intel UHD + GeForce GTX 1650.
- Rust: stable `x86_64-pc-windows-msvc`, **1.99.1** (updated from 1.93.0 on 2026-11-11).
- VS 2022 Community 17.14, MSVC toolset 14.44.35207, Windows SDK 10.0.22621 and 10.0.26100.
  `link.exe`/`cl.exe` are not on PATH (cargo finds them through vswhere).
- Not installed: clang/libclang (`LIBCLANG_PATH` unset), nasm, yasm, vcpkg on PATH, scoop, choco.
  cmake and ninja exist only inside the VS install.
- MSYS2 at `C:\msys64` (ucrt64 gcc 13.2, pkgconf 2.1.0) is on PATH.
- FFmpeg **CLI only**: `ffmpeg`/`ffprobe`/`ffplay` 9.0.2 (gyan.dev full build, winget). No headers,
  import libs or `av*.dll` anywhere, `FFMPEG_DIR` unset. Usable for generating test media.
- No mpv/libmpv, no GStreamer. VLC 3.0.21 runtime without SDK.
- No past sessions about this project.

### Sample media

| File (in `C:\Users\domek\Videos\Captures`) | Size | Duration | Streams |
|---|---|---|---|
| `1 joga.mp4` | 2.2 GB | 28 min 11 s | H.264 Main 1920x1080 60 fps, AAC-LC 48 kHz stereo |
| `Jak Nagrać Ekran Komputera. … 2025-06-06 11-26-29.mp4` | 20 MB | 16.7 s | H.264 Main 1920x1032 60 fps, AAC-LC 48 kHz stereo |

## Audio

### symphonia 0.6.1 (2026-08-13, MPL-2.0, MSRV 1.85) — pure Rust

- Demuxers: MP4/M4A (`isomp4`), MKV/WebM (`mkv`), OGG, WAV, AIFF, CAF, ADTS (in `aac`), MPEG audio.
  No AVI, no MPEG-TS.
- Decoders: FLAC, MP3/MP2/MP1, AAC-LC, ALAC, Vorbis, PCM, ADPCM.
- Default features omit MP3, AAC, ALAC and MP4 — enable `mp3`, `aac`, `isomp4` (or `all`).
- **Gaps:** AAC is LC only (decoder returns `aac too complex` when SBR is signalled explicitly;
  behaviour for implicitly signalled HE-AAC not verified). No Opus decoder (packets are demuxed).
  The README lists `he-aac`/`opus` flags that the published crate does not have.
- 0.6.0 broke the 0.5 API: new audio/metadata/timestamp types, readers return `Ok(None)` at end.
- Seeking: `FormatReader::seek(mode, to)`; decoder must be reset afterwards.
- Non-seekable input: `ReadOnlySource<R: Read>` (`is_seekable() = false`, `byte_len() = None`).
- `symphonia-core` 0.6.1 has `codecs::video` and `codecs::subtitle` parameter types, so the MP4/MKV
  demuxers describe video tracks; symphonia has no video decoders.

### Output

- **cpal 0.18.2** (2026-08-16, Apache-2.0): WASAPI on Windows; unified `Error`/`ErrorKind`; default
  stream follows the system default device (`ErrorKind::DeviceChanged`).
- **rodio 0.23.0** (2026-10-11): `DeviceSinkBuilder::open_default_sink()`, `Player::connect_new`,
  volume/pause/`try_seek`. Caveats: pins **symphonia ^0.5.5** (not 0.6), resampler is linear
  interpolation (rodio issue #584), no rubato.
- **rubato 5.0.1** (2026-10-01): sinc and FFT resamplers; majors change quickly (3→5 in 5 months).

### Filling the gaps (HE-AAC, Opus)

| Crate | Version / date | Kind | Note |
|---|---|---|---|
| `symphonia-adapter-fdk-aac` | 0.2.0, 2026-05-16 | C (libfdk-aac) | HE-AAC for symphonia 0.6; FDK licence has no patent grant |
| `oxideav-aac` | 0.1.7, 2026-09-01, MIT | pure Rust | described as AAC-LC only |
| `symphonia-adapter-libopus` | 0.3.0, 2026-05-16 | C (bundled libopus) | for symphonia 0.6 |
| `opus-decoder` | 0.1.1, 2026-03-12, MIT/Apache | pure Rust | 219k downloads |
| `opus-rs` | 0.1.42, 2026-11-10, BSD-3 | pure Rust | 245k downloads |
| `opus-pure` | 0.2.3, 2026-11-07, BSD-3 | pure Rust | 78k downloads |
| `ruopus` | 0.1.2, 2026-07-01, MIT | pure Rust | used by `symphonia-adapter-opus-rs` 0.1.1 |

No pure-Rust HE-AAC (SBR/PS) decoder was found on crates.io.

### Internet radio

- **stream-download 0.24.5** (2026-10-28, MSRV 1.95): HTTP stream exposed as `Read + Seek`,
  background download on tokio, bounded/adaptive storage for infinite streams, reqwest ^0.13.4.
  Seeking from the end fails on unknown-length streams. No HLS.
- **icy-metadata 0.6.0** (2026-01-10): `request_icy_metadata()` on the reqwest builder,
  `IcyHeaders::parse_from_headers`, `IcyMetadataReader` strips metadata blocks and calls back with
  `stream_title()`. Its `examples/seek.rs` shows stream-download + icy-metadata + rodio.
- **reqwest 0.13.5**: async by default; `blocking` is an opt-in feature; rustls is the default TLS.
- HLS: only playlist parsers (`m3u8-rs` 6.0.1, `hls_m3u8` 0.7.0). No established pure-Rust client
  that feeds segments to a decoder; symphonia has no MPEG-TS demuxer.

Test streams that answered on 2026-11-11:

| URL | Type |
|---|---|
| `https://ice6.somafm.com/insound-128-mp3` | MP3 128k, Icecast |
| `https://ice1.somafm.com/groovesalad-128-mp3` | MP3 128k |
| `https://ice1.somafm.com/groovesalad-128-aac` | AAC 128k (profile not checked) |
| `https://ice1.somafm.com/groovesalad-64-aac` | AAC 64k (profile not checked) |
| `https://us2.internet-radio.com/proxy/mattjohnsonradio?mp=/stream` | MP3 80k, `icy-metaint: 8192` |
| `http://stream.live.vc.bbcmedia.co.uk/bbc_world_service` | MP3 56k, `icy-metaint: 16000` |

## Video

### Pure-Rust decoders (all young; claims are the authors')

| Codec | Crate | Version / date | Licence | Claims |
|---|---|---|---|---|
| H.264 | `rust_h264` | 0.6.1, 2026-11-03 (first 2026-04) | MIT/Apache | Baseline/Main/High/High10/4:2:2, CABAC, B-slices, interlaced, frame threading, SSE2/AVX2, 199/204 JVT vectors, Annex B + AVCC input |
| H.264 | `rusty_h264` | 0.17.2, 2026-11-09 | BSD-2 | Constrained Baseline + most of High; default SIMD needs `nasm` |
| HEVC | `rust_h265` | 0.4.1, 2026-11-03 | MIT/Apache | Main/Main10, 147/147 HEVC_v1 vectors, frame + wavefront threading, no SIMD yet |
| HEVC | `rusty_h265` | 0.7.1, 2026-11-08 | Apache-2.0 | 147/147 vectors, `forbid(unsafe)` core |
| MPEG-4 ASP (Xvid/DivX) | `oxideav-mpeg4video` | 0.1.10, 2026-11-10 | MIT | I/P/B/GMC, qpel, interlaced; FourCCs XVID/DIVX/DX50/FMP4/MP4V |
| MS-MPEG4 (DIV3…) | `oxideav-msmpeg4` | 0.0.11 | MIT | not evaluated |
| MPEG-1/2 | `oxideav-mpeg12video` | 0.0.14 | MIT | completeness unknown |
| VP9 | `rusty_vp9` | 0.1.3, 2026-10-17 | Apache-2.0 | bit-exact on 315 libvpx vectors |
| VP8 | `vp8-decoder` | 0.1.1, 2026-11-06 | — | 200 downloads |
| AV1 | `rav1d` | 1.1.0, 2025-05-07 | BSD-2 | dav1d port, ~5 % slower than C; default `asm` feature needs `nasm`; Windows MSVC not in README target list |
| MJPEG | `zune-jpeg` | 0.5.16-rc3 | — | pure-Rust JPEG |

Not pure Rust: `openh264` 0.9.10 (Cisco C++ source compiled via `cc`), `dav1d` bindings.
NihAV is AGPL and not on crates.io. `oxideav-h264`, `oxideav-vp8/vp9/av1` are scaffolds.

### Pure-Rust demuxers

- MP4: symphonia `isomp4`; `mp4` 0.14.0 (2023, stale); `re_mp4` 0.5.1; `mp4parse` 0.17.0 (metadata only).
- MKV/WebM: symphonia `mkv`; `matroska-demuxer` 0.8.1.
- AVI: `oxideav-avi` 0.0.11 (AVI 1.0, idx1, multi-stream, "usable standalone");
  `rff-format-avi` 0.4.6; the `avi` crate is an empty placeholder.

### Non-Rust backends (for reference; none installed)

- **FFmpeg bindings** (`ffmpeg-next` 9.0.0, `ffmpeg-the-third` 6.0.0+ffmpeg-9.0, `rsmpeg` up to
  FFmpeg 8): need a shared FFmpeg dev package + `FFMPEG_DIR` + libclang for bindgen.
- **libmpv** (`libmpv2` 7.0.1, LGPL): needs prebuilt `libmpv-2.dll` (shinchiro / zhongfly builds,
  MinGW) and an `mpv.lib` generated from a `.def`; render API is OpenGL only in the crate.
- **GStreamer** (`gstreamer` 0.25.4): needs the GStreamer 1.28 MSVC runtime + devel install.
- **Media Foundation** via `windows` 0.62.2: H.264, MPEG-4 ASP, MJPEG inbox; HEVC and AV1 need
  Store codec packs; AC-3 removed in Windows 11 24H2; MKV listed on the newer codec page only.
- `vlc-rs` 0.3.0 dates from 2018.

## Window, UI, presentation

- **egui / eframe 0.37.0** (2026-10-15, MSRV 1.95): wgpu is the default renderer, glow is opt-in
  (`glow` feature + `Renderer::Glow`). Reactive repaint (`request_repaint`, callable from other
  threads; `request_repaint_after`). `TextureHandle::set` for CPU frame upload;
  `egui_wgpu::CallbackTrait` or `register_native_texture` for GPU paths. Drag-and-drop via
  `RawInput::dropped_files`, fullscreen via `ViewportCommand::Fullscreen`. `eframe::Frame`
  exposes the raw window handle.
- **iced 0.14.0** (2025-12-07, "experimental"); `iced_video_player` 0.6.0 is GStreamer-based.
- **slint 1.18.1**: GPL-3 / royalty-free with attribution / commercial.
- **winit** 0.30.13 stable (0.31 is beta and renames much of the API). **wgpu 30.0.1**: only
  `NV12` and `P010` multi-planar formats (DX12/Vulkan, feature-gated); no I420 — planar YUV needs
  three `R8Unorm` textures and a shader. **softbuffer 0.4.8**: CPU blit to a window.
  **pixels 0.17.2** depends on wgpu 29.
- **rfd 0.18.0** native file dialogs; **directories 6.0.0**, **serde 1.0.229**, **toml 1.2.1** for
  settings; **souvlaki 0.9.3** for Windows media keys (needs an HWND).
- Footprint data is thin: near-empty egui app 4.3 MB with glow vs 8.8 MB with wgpu (user report in
  egui issue #5889); no Windows or 2026 benchmarks found for any toolkit.
- Embedding a backend's own window (mpv `--wid`) prevents drawing our controls over the video;
  rendering frames into our own texture avoids that.

## Verified locally (2026-11-11, this machine, release builds)

- **symphonia 0.6.1 hands out video packets**: MP4 and MKV tracks carry H.264/HEVC samples in
  length-prefixed form with the `avcC` / `hvcC` record in `extra_data`. It has no keyframe flag
  and no keyframe-aware seek, so the player finds keyframes by NAL type.
- **`rust_h264` 0.6.1** (`ThreadedDecoder`): 0 errors on both sample captures (1080p, 10 Mbit/s,
  Main profile). Decode speed 29 fps on 1 thread, 70 on 4, 94 on 6, 106 on 12. Output is
  bit-identical to FFmpeg at the pixels checked. No per-frame metadata on the threaded decoder,
  so timestamps are paired through a min-heap (display order = ascending PTS).
- **`rust_h265` 0.4.1**: plays x265 output in MKV and MP4; matches FFmpeg to within 1 level at
  the pixel checked. `PixelData` is not nameable from outside the crate.
- **symphonia's MP4 demuxer rejects an `hvcC` box larger than 1 KB** ("atom size is greater than
  1 kb"). x265 embeds its settings string there by default, so FFmpeg-made HEVC MP4s fail to
  open unless encoded with `-x265-params info=0`. The same stream in MKV is fine.
- **`oxideav-mpeg4video` 0.1.10** (`Mpeg4VideoDecoder`, `DecodeOptions::ecosystem()`): decodes
  FFmpeg's Xvid-tagged MPEG-4 bit-identically at the pixel checked; returns macroblock-padded
  planes (crop to the container's size). `oxideav-avi` was not used — its API is tied to the
  oxideav registry types — and a ~400-line AVI 1.0 demuxer was written instead.
- **`symphonia-adapter-opus-rs` 0.1.1** (ruopus, pure Rust) registers as a symphonia 0.6 decoder
  and plays Ogg Opus.
- **cpal 0.18.2 on WASAPI opens output streams with `AUTOCONVERTPCM`**, so Windows resamples
  any source rate; no resampler crate is needed (rubato was dropped).
- **Radio**: `ureq` 3.5.1 (blocking, rustls) plus a hand-written ICY reader plays SomaFM MP3
  streams and reports titles. stream-download / icy-metadata / tokio were not needed.
- **eframe 0.37 API**: `App::ui(&mut self, ui, frame)` replaces `update`; `NativeOptions` has no
  `vsync` field; `Id::new` is deprecated in favour of `Id::unique`; dropped files expose
  `path()` as a method. Release binary with the glow renderer and all decoders: 14.9 MB.
- **On-screen colour**: the YUV shader's output read back with `glReadPixels` matches FFmpeg's
  RGB within 1 level (BT.601 for SD, BT.709 for HD, limited range).
- The two sample captures are variable-frame-rate, about 30 fps average despite the 60 fps
  nominal rate.

### Learned during testing and review (2026-11-11/12)

- **symphonia MP4/MKV readers cannot be repositioned after reaching end of file** ("no atom
  pending read", "not an ancestor of the current element"); the player reopens the file.
- **symphonia's Matroska seek is unreliable on files with a sparse index** (one keyframe): it
  reports a plausible landing point, returns one packet from there, then continues from the
  start. The player validates the landing by reading ahead and falls back to reading forward.
- **symphonia exposes no AAC priming/padding for MP4 and its AAC decoder ignores packet trims**;
  the player reads the edit list (`moov/trak/edts/elst`) itself and trims decoded samples.
  Times are only exact when the track timescale is a whole multiple of the sample rate.
- **A truncated MP4 or MKV does not play its intact part** with symphonia (reported as an error).
- **`rust_h264::ThreadedDecoder` runs about `threads + 16` pictures behind its input** and
  blocks once `threads` pictures are in flight; its latency in fed pictures varies with feed
  rate. Slice-header errors are returned at once; worker (macroblock) errors surface on a later
  call with no way to tell which packet they belong to.
- **`ring` (pulled in by ureq's default `rustls` feature) compiles C and assembly.** ureq with
  `rustls-no-provider` + `rustls-webpki-roots` and the `rustls-rustcrypto` provider
  (0.1.0-pre.0, unaudited) is pure Rust and plays HTTPS radio.
- **The MSVC target links `VCRUNTIME140.dll` dynamically by default**; `.cargo/config.toml` with
  `-C target-feature=+crt-static` removes that import.
- symphonia retries reads that fail with `ErrorKind::Interrupted` forever; use another kind to
  abort a stream.
- eframe 0.37: `Context::egui_is_using_pointer()` (the unprefixed name is gone).

## Not verified by research

- Whether the young pure-Rust decoders reach real time at 1080p60 on this machine.
- Whether symphonia's MP4/MKV demuxers hand out video packets in usable form (AVCC + extradata).
- AAC profile of the SomaFM AAC streams; behaviour of symphonia on implicit HE-AAC.
- Any pure-Rust HLS radio path.
