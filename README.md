# playr

Light, fast media player for video files, audio files and internet radio, built on
pure-Rust demuxers and decoders. It ships as one executable with no codec DLLs.

## Install

Both installers work per-user and need no administrator rights or sudo. They build playr
with `cargo` (from [rustup](https://rustup.rs)), install it with its icon, and register it
for media files so that **right-click > Open with > playr** works. Run either one again to
update.

### Windows 11

Double-click `install\windows\install.cmd`, or:

```powershell
powershell -ExecutionPolicy Bypass -File install\windows\install.ps1
```

This installs to `%LOCALAPPDATA%\Programs\playr` and adds a Start menu entry. Windows 11 does
not let a program make itself the default player, so if you want playr as the default, choose
it under *Settings > Apps > Default apps > playr*. To uninstall, use *Settings > Apps >
Installed apps*, or run `install.cmd -Uninstall`.

### Linux (Ubuntu)

```sh
bash install/linux/install.sh --install-deps   # --install-deps apt-gets the build packages once (sudo)
```

This installs to `~/.local/bin` and adds a desktop entry and icons under `~/.local/share`.
To make playr the default for a file type, open the file's *Properties > Open With*. To
uninstall, pass `--uninstall`.

Both scripts also accept a binary that is already built: `-Binary path\to\playr.exe` on
Windows, `--binary path/to/playr` on Linux.

## Build and test

```sh
cargo build --release
cargo test
```

The icon is drawn by `assets/make_icon.py` (requires Pillow), which writes `playr.svg`,
`playr.png` and `playr.ico`.
