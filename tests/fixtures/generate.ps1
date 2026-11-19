# Regenerates the media fixtures and their FFmpeg-derived reference data.
#
# The fixtures are checked in so the suite does not need FFmpeg and never skips; run this
# only to rebuild them (FFmpeg 9.x CLI on PATH):  pwsh tests/fixtures/generate.ps1
#
# Every video is testsrc2 320x240 at 25 fps, every audio track a 440 Hz sine (amplitude 1/8,
# which is what lavfi's `sine` produces). The tests derive their expected values from these
# parameters and from the reference files written at the bottom, never from playr's output.

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$ff = @('-hide_banner', '-loglevel', 'error', '-y')
$bitexact = @('-map_metadata', '-1', '-fflags', '+bitexact', '-flags:v', '+bitexact', '-flags:a', '+bitexact')

function Video([double]$secs, [string]$size = '320x240') {
    @('-f', 'lavfi', '-i', "testsrc2=size=${size}:rate=25:duration=$secs")
}
function Sine([double]$secs, [int]$rate = 44100) {
    @('-f', 'lavfi', '-i', "sine=frequency=440:sample_rate=${rate}:duration=$secs")
}
function Run([string[]]$ffArgs) {
    & ffmpeg @ff @ffArgs
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed: $ffArgs" }
}

# --- video + audio -------------------------------------------------------------------------
$x264 = @('-c:v', 'libx264', '-preset', 'veryfast', '-pix_fmt', 'yuv420p')
$aac = @('-c:a', 'aac', '-b:a', '96k')

Run ((Video 2) + (Sine 2) + $x264 + @('-bf', '0', '-g', '25') + $aac + $bitexact + "$here\h264_aac.mp4")
# x264 defaults: B-frames, so packets are not in presentation order.
Run ((Video 2) + (Sine 2) + $x264 + $aac + $bitexact + "$here\h264_bframes_aac.mp4")
# One keyframe every 5 s (0 s and 5 s) for the seek tests.
Run ((Video 10 '160x120') + (Sine 10) + $x264 + @('-bf', '0', '-g', '125', '-keyint_min', '125', '-sc_threshold', '0') + $aac + $bitexact + "$here\h264_longgop_aac.mp4")
# Full HD with B-frames: pictures that take real time to decode, for the pipeline tests.
Run ((Video 3 '1920x1080') + (Sine 3) + $x264 + $aac + $bitexact + "$here\h264_1080p_aac.mp4")
# 16 s of picture (400 packets, a keyframe every second) with only 2 s of sound: the clock has
# to carry on without audio and the reader runs far ahead looking for more of it.
Run ((Video 16 '160x120') + (Sine 2) + $x264 + @('-bf', '0', '-g', '25') + $aac + $bitexact + "$here\h264_16s_audio_2s.mp4")
# The same picture with the sound arriving only at 14 s, 350 video packets into the file.
Run ((Video 16 '160x120') + @('-itsoffset', '14') + (Sine 2) + $x264 + @('-bf', '0', '-g', '25') + $aac + $bitexact + "$here\h264_16s_audio_from_14s.mp4")
# Short groups of pictures: every picture an IDR, and an IDR every fourth picture.
Run ((Video 4) + (Sine 4) + $x264 + @('-g', '1') + $aac + $bitexact + "$here\h264_intra_aac.mp4")
Run ((Video 4) + (Sine 4) + $x264 + @('-g', '4', '-keyint_min', '4', '-sc_threshold', '0') + $aac + $bitexact + "$here\h264_gop4_aac.mp4")
Run ((Video 4) + (Sine 4) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error:info=0:keyint=1', '-tag:v', 'hvc1') + $aac + $bitexact + "$here\hevc_intra_aac.mp4")
# Open GOPs: random-access pictures are marked by a recovery-point SEI instead of being IDR.
Run ((Video 4) + (Sine 4) + $x264 + @('-x264-params', 'open-gop=1:keyint=25:min-keyint=25:scenecut=0') + $aac + $bitexact + "$here\h264_opengop_aac.mp4")
# The index (moov) in front of the media data, so a cut-off copy still opens.
Run ((Video 4) + (Sine 4) + $x264 + @('-bf', '0', '-g', '25', '-movflags', '+faststart') + $aac + $bitexact + "$here\h264_faststart_aac.mp4")
Run ((Video 2) + (Sine 2) + $x264 + @('-c:a', 'libvorbis', '-ac', '2') + $bitexact + "$here\h264_vorbis.mkv")
Run ((Video 2) + (Sine 2) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error', '-c:a', 'libvorbis', '-ac', '2') + $bitexact + "$here\hevc_vorbis.mkv")
Run ((Video 2) + (Sine 2) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error:info=0', '-tag:v', 'hvc1') + $aac + $bitexact + "$here\hevc_aac.mp4")
# Without info=0 x265 embeds a long SEI in hvcC (> 1 KB), the documented symphonia limit.
Run ((Video 2) + (Sine 2) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error', '-tag:v', 'hvc1') + $aac + "$here\hevc_big_hvcc_aac.mp4")
$xvid = @('-c:v', 'mpeg4', '-vtag', 'XVID', '-q:v', '4', '-g', '12', '-bf', '0', '-pix_fmt', 'yuv420p')
Run ((Video 2) + (Sine 2) + $xvid + @('-c:a', 'libmp3lame', '-b:a', '128k', '-ac', '2') + $bitexact + "$here\xvid_mp3.avi")
Run ((Video 2) + (Sine 2) + $xvid + @('-c:a', 'pcm_s16le', '-ac', '2') + $bitexact + "$here\xvid_pcm16.avi")
Run ((Video 2) + (Sine 2) + $xvid + @('-c:a', 'pcm_u8') + $bitexact + "$here\xvid_pcm8.avi")

# --- unsupported video codecs --------------------------------------------------------------
Run ((Video 2) + (Sine 2) + @('-c:v', 'mjpeg', '-q:v', '8', '-pix_fmt', 'yuvj420p', '-c:a', 'pcm_s16le') + $bitexact + "$here\mjpeg_pcm16.avi")
Run ((Video 2) + (Sine 2) + @('-c:v', 'libvpx-vp9', '-b:v', '200k', '-deadline', 'realtime', '-cpu-used', '8', '-c:a', 'libvorbis', '-ac', '2') + $bitexact + "$here\vp9_vorbis.mkv")
# Neither track has a decoder: Motion JPEG picture, AC-3 sound.
Run ((Video 2) + (Sine 2 48000) + @('-c:v', 'mjpeg', '-q:v', '8', '-pix_fmt', 'yuvj420p', '-c:a', 'ac3', '-b:a', '96k') + $bitexact + "$here\mjpeg_ac3.avi")

# --- audio only (3 s) ----------------------------------------------------------------------
Run ((Sine 3) + @('-c:a', 'flac', '-ac', '2') + $bitexact + "$here\sine.flac")
Run ((Sine 3) + @('-c:a', 'libmp3lame', '-b:a', '128k', '-ac', '2') + $bitexact + "$here\sine.mp3")
Run ((Sine 3) + $aac + @('-f', 'adts') + $bitexact + "$here\sine.aac")
Run ((Sine 3) + $aac + @('-ac', '2') + $bitexact + "$here\sine.m4a")
Run ((Sine 3) + @('-c:a', 'libvorbis', '-ac', '2') + $bitexact + "$here\sine.ogg")
Run ((Sine 3 48000) + @('-c:a', 'libopus', '-b:a', '64k', '-ac', '2') + $bitexact + "$here\sine.opus")
Run ((Sine 3) + @('-c:a', 'pcm_s16le') + $bitexact + "$here\sine.wav")
# 4 s on the clock with the second from 1.5 s to 2.5 s missing: packet timestamps jump there.
Run ((Sine 4) + @('-af', "aselect='not(between(t,1.5,2.5))'", '-c:a', 'libvorbis', '-ac', '2') + $bitexact + "$here\sine_gap.mka")
# Bare MPEG frames (no ID3 tag, no Xing header), as an Icecast server would send them.
Run ((Sine 4) + @('-c:a', 'libmp3lame', '-b:a', '64k', '-id3v2_version', '0', '-write_xing', '0') + $bitexact + "$here\radio.mp3")

# --- reference data from FFmpeg's own decoders ---------------------------------------------
# <name>.framecrc: Adler-32 and timestamp of every decoded picture (planar yuv420p).
# <name>.f0.yuv / .f37.yuv: the raw planes of pictures 0 and 37 (an inter-coded one).
foreach ($f in 'h264_aac.mp4', 'h264_bframes_aac.mp4', 'h264_vorbis.mkv', 'hevc_vorbis.mkv', 'hevc_aac.mp4', 'hevc_big_hvcc_aac.mp4', 'xvid_mp3.avi', 'xvid_pcm16.avi') {
    Run @('-i', "$here\$f", '-map', '0:v:0', '-pix_fmt', 'yuv420p', '-fps_mode', 'passthrough', '-f', 'framecrc', "$here\$f.framecrc")
    Run @('-i', "$here\$f", '-map', '0:v:0', '-vf', 'select=eq(n\,0)', '-frames:v', '1', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', "$here\$f.f0.yuv")
    Run @('-i', "$here\$f", '-map', '0:v:0', '-vf', 'select=eq(n\,37)', '-frames:v', '1', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', "$here\$f.f37.yuv")
}
foreach ($f in 'h264_longgop_aac.mp4', 'h264_1080p_aac.mp4', 'h264_16s_audio_2s.mp4', 'h264_opengop_aac.mp4', 'h264_faststart_aac.mp4', 'h264_16s_audio_from_14s.mp4', 'h264_intra_aac.mp4', 'h264_gop4_aac.mp4', 'hevc_intra_aac.mp4') {
    Run @('-i', "$here\$f", '-map', '0:v:0', '-pix_fmt', 'yuv420p', '-fps_mode', 'passthrough', '-f', 'framecrc', "$here\$f.framecrc")
}

Get-ChildItem $here -File | Sort-Object Name | Format-Table Name, Length
