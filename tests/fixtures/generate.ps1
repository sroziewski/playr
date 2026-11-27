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
# The same picture with sound all the way: 400 video packets, more than the reader may stash
# (300) while nobody takes the decoded pictures off the queue.
Run ((Video 16 '160x120') + (Sine 16) + $x264 + @('-bf', '0', '-g', '25') + $aac + $bitexact + "$here\h264_16s_aac.mp4")
# 2 s of picture with 1 s of sound: the end of the file is decided by its last picture.
Run ((Video 2 '160x120') + (Sine 1) + $x264 + @('-bf', '0', '-g', '25') + $aac + $bitexact + "$here\h264_2s_audio_1s.mp4")
# Short groups of pictures: every picture an IDR, and an IDR every fourth picture.
Run ((Video 4) + (Sine 4) + $x264 + @('-g', '1') + $aac + $bitexact + "$here\h264_intra_aac.mp4")
Run ((Video 4) + (Sine 4) + $x264 + @('-g', '4', '-keyint_min', '4', '-sc_threshold', '0') + $aac + $bitexact + "$here\h264_gop4_aac.mp4")
Run ((Video 4) + (Sine 4) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error:info=0:keyint=1', '-tag:v', 'hvc1') + $aac + $bitexact + "$here\hevc_intra_aac.mp4")
# Open GOPs: random-access pictures are marked by a recovery-point SEI instead of being IDR.
Run ((Video 4) + (Sine 4) + $x264 + @('-x264-params', 'open-gop=1:keyint=25:min-keyint=25:scenecut=0') + $aac + $bitexact + "$here\h264_opengop_aac.mp4")
# The index (moov) in front of the media data, so a cut-off copy still opens.
Run ((Video 4) + (Sine 4) + $x264 + @('-bf', '0', '-g', '25', '-movflags', '+faststart') + $aac + $bitexact + "$here\h264_faststart_aac.mp4")
Run ((Video 12 '160x120') + $x264 + @('-bf', '0', '-g', '50', '-keyint_min', '50', '-sc_threshold', '0', '-an', '-movflags', '+dash+global_sidx') + $bitexact + "$here\dash_video.mp4")
Run ((Sine 12) + $aac + @('-vn', '-frag_duration', '3000000', '-movflags', '+dash+global_sidx') + $bitexact + "$here\dash_audio.m4a")
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

# --- still pictures ------------------------------------------------------------------------
# One JPEG of a single colour, RGB (60, 140, 220), 50x30 (not a whole number of 8x8 blocks),
# for the YouTube browser's thumbnail decoder.
Run @('-f', 'lavfi', '-i', 'color=c=0x3C8CDC:size=50x30:duration=1', '-frames:v', '1', '-c:v', 'mjpeg', '-q:v', '2', '-pix_fmt', 'yuvj444p', '-update', '1', '-map_metadata', '-1', '-fflags', '+bitexact', '-flags:v', '+bitexact', "$here\solid_50x30.jpg")

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

# --- HLS (tests/hls_*.rs) ------------------------------------------------------------------
# 160x120 picture with a keyframe every second and no B-frames, cut into 1 s segments, so
# segment n holds pictures 25n..25n+24 and starts at n seconds.
$hlsRoot = "$here\hls"
$gop1s = @('-bf', '0', '-g', '25', '-keyint_min', '25', '-sc_threshold', '0')
$hlsVod = @('-f', 'hls', '-hls_time', '1', '-hls_playlist_type', 'vod')
function Hls([string]$name, [string[]]$encode, [string[]]$extra = @(), [string]$ext = 'ts') {
    $dir = "$hlsRoot\$name"
    if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    New-Item -ItemType Directory -Force $dir | Out-Null
    # FFmpeg writes the fMP4 init section relative to the working directory.
    Push-Location $dir
    try { Run ($encode + $bitexact + $hlsVod + $extra + @('-hls_segment_filename', "seg%d.$ext", 'index.m3u8')) } finally { Pop-Location }
}
# H.264 + AAC in MPEG-TS, 6 segments (also played as a live stream that grows).
Hls 'ts' ((Video 6 '160x120') + (Sine 6) + $x264 + $gop1s + $aac)
# HEVC + AAC in MPEG-TS, 2 segments.
Hls 'hevc' ((Video 2 '160x120') + (Sine 2) + @('-c:v', 'libx265', '-preset', 'veryfast', '-pix_fmt', 'yuv420p', '-x265-params', 'log-level=error:info=0:keyint=25:min-keyint=25:scenecut=0:bframes=0') + $aac)
# Fragmented MP4 after an EXT-X-MAP header, 4 segments.
Hls 'fmp4' ((Video 4 '160x120') + (Sine 4) + $x264 + $gop1s + $aac) @('-hls_segment_type', 'fmp4', '-hls_fmp4_init_filename', 'init.mp4') 'm4s'
# Picture only and sound only, for a master playlist with a separate audio rendition.
Hls 'video_only' ((Video 4 '160x120') + $x264 + $gop1s + @('-an'))
Hls 'audio_only' ((Sine 4) + $aac + @('-vn'))
# MP3 sound only in MPEG-TS, 2 segments.
Hls 'mp3_ts' ((Sine 2) + @('-c:a', 'libmp3lame', '-b:a', '64k', '-vn'))
# AES-128 encrypted by FFmpeg: key.bin is 2b7e151628aed2a6abf7158809cf4f3c (the FIPS-197 example
# key), the IV 000102030405060708090a0b0c0d0e0f is named in the playlist. (FFmpeg always writes
# the IV, so the tests encrypt their own segments for the IV-from-sequence-number case.)
$hlsKey = [byte[]](0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c)
$keyInfo = Join-Path ([IO.Path]::GetTempPath()) 'playr_hls_aes.keyinfo'
$keyFile = Join-Path ([IO.Path]::GetTempPath()) 'playr_hls_aes.key'
[IO.File]::WriteAllBytes($keyFile, $hlsKey)
Set-Content -Path $keyInfo -Value @('key.bin', $keyFile, '000102030405060708090a0b0c0d0e0f') -Encoding ascii
Hls 'aes' ((Video 2 '160x120') + (Sine 2) + $x264 + $gop1s + $aac) @('-hls_key_info_file', $keyInfo)
[IO.File]::WriteAllBytes("$hlsRoot\aes\key.bin", $hlsKey)
Remove-Item $keyInfo, $keyFile

# --- reference data from FFmpeg's own decoders ---------------------------------------------
# <name>.framecrc: Adler-32 and timestamp of every decoded picture (planar yuv420p).
# <name>.f0.yuv / .f37.yuv: the raw planes of pictures 0 and 37 (an inter-coded one).
foreach ($f in 'h264_aac.mp4', 'h264_bframes_aac.mp4', 'h264_vorbis.mkv', 'hevc_vorbis.mkv', 'hevc_aac.mp4', 'hevc_big_hvcc_aac.mp4', 'xvid_mp3.avi', 'xvid_pcm16.avi') {
    Run @('-i', "$here\$f", '-map', '0:v:0', '-pix_fmt', 'yuv420p', '-fps_mode', 'passthrough', '-f', 'framecrc', "$here\$f.framecrc")
    Run @('-i', "$here\$f", '-map', '0:v:0', '-vf', 'select=eq(n\,0)', '-frames:v', '1', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', "$here\$f.f0.yuv")
    Run @('-i', "$here\$f", '-map', '0:v:0', '-vf', 'select=eq(n\,37)', '-frames:v', '1', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', "$here\$f.f37.yuv")
}
foreach ($f in 'h264_longgop_aac.mp4', 'h264_1080p_aac.mp4', 'h264_16s_audio_2s.mp4', 'h264_opengop_aac.mp4', 'h264_faststart_aac.mp4', 'h264_16s_audio_from_14s.mp4', 'h264_16s_aac.mp4', 'h264_2s_audio_1s.mp4', 'h264_intra_aac.mp4', 'h264_gop4_aac.mp4', 'hevc_intra_aac.mp4') {
    Run @('-i', "$here\$f", '-map', '0:v:0', '-pix_fmt', 'yuv420p', '-fps_mode', 'passthrough', '-f', 'framecrc', "$here\$f.framecrc")
}

Get-ChildItem $here -File | Sort-Object Name | Format-Table Name, Length
