//! playr — a light media player built on pure-Rust demuxers and decoders.
//!
//! * [`demux`] / [`avi`] turn files and streams into packets,
//! * [`vdec`] and symphonia decode them,
//! * [`player`] runs the pipeline thread and owns the playback clock,
//! * [`audio_out`] plays sound, [`video_gl`] draws pictures, [`ui`] is the window,
//! * [`net`] reads internet radio and other HTTP sources.

pub mod audio_out;
pub mod avi;
pub mod bitrate;
pub mod browse;
pub mod controls;
pub mod cookies;
pub mod corner;
pub mod dash;
pub mod history;
pub mod hls;
pub mod lists;
pub mod demux;
pub mod media;
pub mod mp4_hvcc;
pub mod net;
pub mod player;
pub mod privacy;
pub mod podcast;
pub mod radio;
pub mod sites;
pub mod spotify;
pub mod stretch;
pub mod subtitles;
pub mod ts;
pub mod ui;
pub mod ui_browse;
pub mod vdec;
pub mod video_gl;
pub mod youtube;
