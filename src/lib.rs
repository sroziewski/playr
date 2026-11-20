//! playr — a light media player built on pure-Rust demuxers and decoders.
//!
//! * [`demux`] / [`avi`] turn files and streams into packets,
//! * [`vdec`] and symphonia decode them,
//! * [`player`] runs the pipeline thread and owns the playback clock,
//! * [`audio_out`] plays sound, [`video_gl`] draws pictures, [`ui`] is the window,
//! * [`net`] reads internet radio and other HTTP sources.

pub mod audio_out;
pub mod avi;
pub mod controls;
pub mod demux;
pub mod media;
pub mod mp4_hvcc;
pub mod net;
pub mod player;
pub mod radio;
pub mod ui;
pub mod vdec;
pub mod video_gl;
