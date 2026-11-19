//! The installers in `install/`: their file-type lists against `ui::MEDIA_EXTENSIONS`, the
//! Windows script run under Windows PowerShell 5.1 against a scratch registry key, and the
//! Linux script run in an `ubuntu:24.04` container.
//!
//! The Windows tests never touch the real `HKCU\Software`: every run passes
//! `-RegistryRoot HKCU:\Software\playr-test-<unique>` and temp folders, and deletes the key.
//! The Linux tests need Docker and network access for apt, so they are ignored by default:
//! `cargo test --test installers -- --include-ignored`.

use std::path::Path;

use playr::ui::MEDIA_EXTENSIONS;

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read_repo_file(relative: &str) -> String {
    let path = repo().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The items of `$Extensions = @('mp4', ...)` in install.ps1.
fn windows_script_extensions(script: &str) -> Vec<String> {
    let line = script
        .lines()
        .find(|line| line.trim_start().starts_with("$Extensions = @("))
        .expect("install.ps1 has a `$Extensions = @(...)` line");
    let list = line.split_once("@(").unwrap().1.rsplit_once(')').expect("the @( list closes on its line").0;
    list.split(',').map(|item| item.trim().trim_matches('\'').to_string()).collect()
}

/// The `"ext:type type ..."` entries of the `EXTENSION_MIME_TYPES=( ... )` array in install.sh.
fn linux_script_entries(script: &str) -> Vec<(String, Vec<String>)> {
    let start = script.find("EXTENSION_MIME_TYPES=(\n").expect("install.sh has an EXTENSION_MIME_TYPES=( array");
    let body = &script[start..];
    let end = body.find("\n)").expect("the EXTENSION_MIME_TYPES array closes");
    body[..end]
        .lines()
        .skip(1)
        .map(|line| {
            let (ext, types) = line.trim().trim_matches('"').split_once(':').expect("an entry is \"ext:types\"");
            (ext.to_string(), types.split_whitespace().map(str::to_string).collect())
        })
        .collect()
}

fn linux_script_extensions(script: &str) -> Vec<String> {
    linux_script_entries(script).into_iter().map(|(ext, _)| ext).collect()
}

#[test]
fn the_windows_installer_registers_exactly_the_media_extensions_of_the_open_dialog() {
    let script = read_repo_file("install/windows/install.ps1");
    assert_eq!(windows_script_extensions(&script), MEDIA_EXTENSIONS);
}

#[test]
fn the_linux_installer_maps_exactly_the_media_extensions_of_the_open_dialog() {
    let script = read_repo_file("install/linux/install.sh");
    assert_eq!(linux_script_extensions(&script), MEDIA_EXTENSIONS);
}

#[test]
fn the_list_readers_see_an_added_or_dropped_extension() {
    let ps = "x\n$Extensions = @('mp4', 'mkv', 'flac')\ny\n";
    assert_eq!(windows_script_extensions(ps), ["mp4", "mkv", "flac"]);
    let sh = "x\nEXTENSION_MIME_TYPES=(\n    \"mp4:video/mp4\"\n    \"ogg:audio/ogg video/ogg\"\n)\ny\n";
    assert_eq!(linux_script_extensions(sh), ["mp4", "ogg"]);
    assert_eq!(
        linux_script_entries(sh),
        [("mp4".to_string(), vec!["video/mp4".to_string()]), ("ogg".to_string(), vec!["audio/ogg".to_string(), "video/ogg".to_string()])]
    );
}

// ---------------------------------------------------------------------------------------------
// Windows: install.ps1 under powershell.exe (Windows PowerShell 5.1, what Windows 11 runs).

#[cfg(windows)]
mod windows {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Output, Stdio};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    const DESCRIPTION: &str = "Light, fast media player: video files, audio files and internet radio";

    /// Prints every key and value under HKCU\<SubKey>, one per line, paths relative to it:
    /// `K<tab>path` and `V<tab>path<tab>name<tab>kind<tab>data`. Uses the .NET registry API, not
    /// the provider cmdlets the installer uses, so a quirk of those does not hide in both.
    const SNAPSHOT_PS: &str = r#"param([string]$SubKey)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$top = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($SubKey)
if ($null -eq $top) { throw "no key HKCU\$SubKey" }
$base = $top.Name.Length
function Write-Key($key) {
    $rel = $key.Name.Substring($base).TrimStart('\')
    "K`t$rel"
    foreach ($n in $key.GetValueNames()) {
        $v = $key.GetValue($n, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($v -is [array]) { $v = $v -join '|' }
        "V`t$rel`t$n`t$($key.GetValueKind($n))`t$v"
    }
    foreach ($s in $key.GetSubKeyNames()) { $c = $key.OpenSubKey($s); Write-Key $c; $c.Close() }
}
Write-Key $top
"#;

    /// Creates HKCU\<SubKey>; with -Seed, also what other programs typically have there.
    const SEED_PS: &str = r#"param([string]$SubKey, [switch]$Seed)
$ErrorActionPreference = 'Stop'
$root = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($SubKey)
function Set-Seed([string]$Path, [string]$Name, [string]$Value) { $k = $root.CreateSubKey($Path); $k.SetValue($Name, $Value); $k.Close() }
if ($Seed) {
    Set-Seed 'Classes\.mp4' '' 'VLC.mp4'
    Set-Seed 'Classes\.mp4' 'Content Type' 'video/mp4'
    Set-Seed 'Classes\.mp4\OpenWithProgids' 'VLC.mp4' ''
    Set-Seed 'Classes\.mp4\OpenWithProgids' 'WMP11.AssocFile.MP4' ''
    Set-Seed 'Classes\.mkv\OpenWithProgids' 'VLC.mkv' ''
    Set-Seed 'Classes\.avi' '' 'VLC.avi'
    $root.CreateSubKey('Classes\.m3u8\ShellNew').Close()
    Set-Seed 'Classes\VLC.mp4' '' 'VLC media file (.mp4)'
    Set-Seed 'Classes\Applications\vlc.exe\shell\open\command' '' '"C:\VLC\vlc.exe" "%1"'
    Set-Seed 'RegisteredApplications' 'VLC' 'Software\Clients\Media\VLC\Capabilities'
    Set-Seed 'Microsoft\Windows\CurrentVersion\Uninstall\VLC' 'DisplayName' 'VLC media player'
    Set-Seed 'Microsoft\Windows\CurrentVersion\App Paths\vlc.exe' '' 'C:\VLC\vlc.exe'
}
$root.Close()
"#;

    const DELETE_PS: &str = r#"param([string]$SubKey)
[Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($SubKey, $false)
"#;

    const SHORTCUT_PS: &str = r#"param([string]$Path)
$l = (New-Object -ComObject WScript.Shell).CreateShortcut($Path)
$l.TargetPath; $l.WorkingDirectory; $l.IconLocation; $l.Description
"#;

    const VERSION_PS: &str = r#"param([string]$Path)
(Get-Item -LiteralPath $Path).VersionInfo.ProductVersion
"#;

    fn helper(name: &str) -> PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        let dir = DIR.get_or_init(|| {
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("installer-helpers-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create helper dir");
            for (file, body) in [
                ("snapshot.ps1", SNAPSHOT_PS),
                ("seed.ps1", SEED_PS),
                ("delete.ps1", DELETE_PS),
                ("shortcut.ps1", SHORTCUT_PS),
                ("version.ps1", VERSION_PS),
            ] {
                std::fs::write(dir.join(file), body).expect("write helper script");
            }
            dir
        });
        dir.join(name)
    }

    fn powershell() -> Command {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
        command
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace("\r\n", "\n")
    }

    fn run_helper(name: &str, args: &[&str]) -> String {
        let out = powershell().arg("-File").arg(helper(name)).args(args).output().expect("start powershell.exe");
        assert!(out.status.success(), "{name} {args:?} failed: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn installer() -> PathBuf {
        repo().join("install").join("windows").join("install.ps1")
    }

    struct Ran {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    }

    fn ran(out: Output) -> Ran {
        Ran { code: out.status.code(), stdout: text(&out.stdout), stderr: text(&out.stderr) }
    }

    /// The message of the terminating error PowerShell printed: the lines before its "At ..."
    /// position line, joined again because the console host wraps long messages.
    fn error_message(stderr: &str) -> String {
        stderr.lines().take_while(|line| !line.starts_with("At ")).collect()
    }

    /// A scratch registry root under HKCU\Software and a scratch folder, both deleted on drop.
    struct Scratch {
        sub_key: String,
        dir: PathBuf,
        /// One scratch at a time: run in parallel, New-Item in one test failed with "No more
        /// data is available" while another test deleted its sibling key under HKCU\Software.
        /// A user never runs two installs at once, so the tests do not either.
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new(tag: &str, seed: bool) -> Self {
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
            let unique = format!("{tag}-{}-{nanos}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
            let scratch = Scratch {
                sub_key: format!("Software\\playr-test-{unique}"),
                dir: Path::new(env!("CARGO_TARGET_TMPDIR")).join("installers").join(&unique),
                _serial: serial,
            };
            for dir in ["build", "Programs O'Neil", "Start Menu\\Programs"] {
                std::fs::create_dir_all(scratch.dir.join(dir)).expect("create scratch folder");
            }
            let mut args = vec![scratch.sub_key.as_str()];
            if seed {
                args.push("-Seed");
            }
            run_helper("seed.ps1", &args);
            scratch
        }

        fn root(&self) -> String {
            format!("HKCU:\\{}", self.sub_key)
        }

        fn install_dir(&self) -> PathBuf {
            self.dir.join("Programs O'Neil").join("playr")
        }

        fn start_menu(&self) -> PathBuf {
            self.dir.join("Start Menu").join("Programs")
        }

        fn exe(&self) -> PathBuf {
            self.install_dir().join("playr.exe")
        }

        /// A stand-in for a built playr.exe: the installer only copies it.
        fn fake_binary(&self) -> PathBuf {
            let path = self.dir.join("build").join("playr-built.exe");
            std::fs::write(&path, b"MZ stand-in for playr.exe, built in this test").expect("write fake binary");
            path
        }

        /// A copy of ping.exe, which carries version information and can run hidden for a while.
        fn ping_binary(&self) -> PathBuf {
            let system = std::env::var("SystemRoot").expect("SystemRoot");
            let path = self.dir.join("build").join("playr-built.exe");
            std::fs::copy(Path::new(&system).join("System32").join("PING.EXE"), &path).expect("copy ping.exe");
            path
        }

        fn run_script(&self, args: &[&std::ffi::OsStr]) -> Ran {
            ran(powershell().arg("-File").arg(installer()).args(args).output().expect("start powershell.exe"))
        }

        fn install_with(&self, binary: &Path, install_dir: &Path, start_menu: &Path, root: &str) -> Ran {
            self.run_script(&[
                "-Binary".as_ref(),
                binary.as_os_str(),
                "-InstallDir".as_ref(),
                install_dir.as_os_str(),
                "-StartMenuDir".as_ref(),
                start_menu.as_os_str(),
                "-RegistryRoot".as_ref(),
                root.as_ref(),
            ])
        }

        fn install(&self, binary: &Path) -> Ran {
            self.install_with(binary, &self.install_dir(), &self.start_menu(), &self.root())
        }

        fn uninstall(&self) -> Ran {
            self.run_script(&[
                "-Uninstall".as_ref(),
                "-InstallDir".as_ref(),
                self.install_dir().as_os_str(),
                "-StartMenuDir".as_ref(),
                self.start_menu().as_os_str(),
                "-RegistryRoot".as_ref(),
                self.root().as_ref(),
            ])
        }

        fn registry(&self) -> BTreeSet<String> {
            run_helper("snapshot.ps1", &[&self.sub_key]).lines().map(str::to_string).collect()
        }

        fn value(&self, path: &str, name: &str) -> String {
            let prefix = format!("V\t{path}\t{name}\t");
            let registry = self.registry();
            let line = registry.iter().find(|line| line.starts_with(&prefix)).unwrap_or_else(|| panic!("no value {path} : {name}"));
            line.splitn(5, '\t').nth(4).unwrap().to_string()
        }

        /// Every folder and file under the scratch folder, relative to it.
        fn files(&self) -> BTreeSet<String> {
            fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
                for entry in std::fs::read_dir(dir).expect("read scratch folder") {
                    let path = entry.expect("folder entry").path();
                    out.insert(path.strip_prefix(base).unwrap().display().to_string());
                    if path.is_dir() {
                        walk(base, &path, out);
                    }
                }
            }
            let mut out = BTreeSet::new();
            walk(&self.dir, &self.dir, &mut out);
            out
        }

        /// Runs the UninstallString the install registered, the way Settings > Installed apps does.
        fn run_uninstall_string(&self) -> Ran {
            let command = self.value("Microsoft\\Windows\\CurrentVersion\\Uninstall\\playr", "UninstallString");
            let rest = command.strip_prefix("powershell.exe ").unwrap_or_else(|| panic!("UninstallString does not start with powershell.exe: {command}"));
            ran(Command::new("powershell.exe").raw_arg(rest).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output().expect("start powershell.exe"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            assert!(self.sub_key.starts_with("Software\\playr-test-"), "refusing to delete {}", self.sub_key);
            let deleted = powershell().arg("-File").arg(helper("delete.ps1")).arg(&self.sub_key).output();
            if !matches!(&deleted, Ok(out) if out.status.success()) {
                eprintln!("could not delete scratch key HKCU\\{}: {deleted:?}", self.sub_key);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A process started from a path, killed on drop.
    struct Running(Child);

    impl Running {
        fn start(exe: &Path) -> Self {
            Running(
                Command::new(exe)
                    .args(["-n", "600", "127.0.0.1"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .unwrap_or_else(|e| panic!("start {}: {e}", exe.display())),
            )
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn add_key(set: &mut BTreeSet<String>, path: &str) {
        set.insert("K\t".to_string());
        let mut so_far = String::new();
        for part in path.split('\\') {
            if !so_far.is_empty() {
                so_far.push('\\');
            }
            so_far.push_str(part);
            set.insert(format!("K\t{so_far}"));
        }
    }

    fn add_value(set: &mut BTreeSet<String>, path: &str, name: &str, kind: &str, data: &str) {
        add_key(set, path);
        set.insert(format!("V\t{path}\t{name}\t{kind}\t{data}"));
    }

    /// Every key and value the install must write, derived from the acceptance criteria:
    /// ProgID with icon and `"<exe>" "%1"`, Applications\playr.exe with SupportedTypes,
    /// OpenWithProgids per extension, Capabilities + RegisteredApplications (path relative to
    /// HKCU), App Paths, and the uninstall entry that re-runs the copied script.
    fn installed_registry(scratch: &Scratch, version: Option<&str>) -> BTreeSet<String> {
        let exe = scratch.exe().display().to_string();
        let install_dir = scratch.install_dir().display().to_string();
        let icon = format!("\"{exe}\",0");
        let open = format!("\"{exe}\" \"%1\"");
        let mut set = BTreeSet::new();
        let s = &mut set;
        add_value(s, "Classes\\playr.media", "", "String", "Media file");
        add_value(s, "Classes\\playr.media", "FriendlyTypeName", "String", "Media file (playr)");
        add_value(s, "Classes\\playr.media\\DefaultIcon", "", "String", &icon);
        add_value(s, "Classes\\playr.media\\shell", "", "String", "open");
        add_value(s, "Classes\\playr.media\\shell\\open", "FriendlyAppName", "String", "playr");
        add_value(s, "Classes\\playr.media\\shell\\open\\command", "", "String", &open);
        add_value(s, "Classes\\Applications\\playr.exe", "FriendlyAppName", "String", "playr");
        add_value(s, "Classes\\Applications\\playr.exe\\DefaultIcon", "", "String", &icon);
        add_value(s, "Classes\\Applications\\playr.exe\\shell\\open\\command", "", "String", &open);
        add_value(s, "playr\\Capabilities", "ApplicationName", "String", "playr");
        add_value(s, "playr\\Capabilities", "ApplicationDescription", "String", DESCRIPTION);
        add_value(s, "playr\\Capabilities", "ApplicationIcon", "String", &icon);
        add_value(s, "RegisteredApplications", "playr", "String", &format!("{}\\playr\\Capabilities", scratch.sub_key));
        for ext in MEDIA_EXTENSIONS {
            add_value(s, "Classes\\Applications\\playr.exe\\SupportedTypes", &format!(".{ext}"), "String", "");
            add_value(s, "playr\\Capabilities\\FileAssociations", &format!(".{ext}"), "String", "playr.media");
            add_value(s, &format!("Classes\\.{ext}\\OpenWithProgids"), "playr.media", "String", "");
        }
        add_value(s, "Microsoft\\Windows\\CurrentVersion\\App Paths\\playr.exe", "", "String", &exe);
        let uninstall = "Microsoft\\Windows\\CurrentVersion\\Uninstall\\playr";
        add_value(s, uninstall, "DisplayName", "String", "playr");
        add_value(s, uninstall, "DisplayIcon", "String", &icon);
        add_value(s, uninstall, "Publisher", "String", "playr");
        if let Some(version) = version {
            add_value(s, uninstall, "DisplayVersion", "String", version);
        }
        add_value(s, uninstall, "InstallLocation", "String", &install_dir);
        let uninstall_string = format!(
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{install_dir}\\uninstall.ps1\" -Uninstall -InstallDir \"{install_dir}\" -StartMenuDir \"{}\" -RegistryRoot \"{}\"",
            scratch.start_menu().display(),
            scratch.root()
        );
        add_value(s, uninstall, "UninstallString", "String", &uninstall_string);
        add_value(s, uninstall, "NoModify", "DWord", "1");
        add_value(s, uninstall, "NoRepair", "DWord", "1");
        set
    }

    fn install_stdout(scratch: &Scratch) -> String {
        format!(
            "playr is installed in {}.\nRight-click a media file > Open with > playr. To make playr the default player,\nchoose it in Settings > Apps > Default apps > playr.\n",
            scratch.install_dir().display()
        )
    }

    fn installed_files(before: &BTreeSet<String>) -> BTreeSet<String> {
        let mut files = before.clone();
        for added in ["Programs O'Neil\\playr", "Programs O'Neil\\playr\\playr.exe", "Programs O'Neil\\playr\\uninstall.ps1", "Start Menu\\Programs\\playr.lnk"] {
            files.insert(added.to_string());
        }
        files
    }

    fn assert_succeeded(what: &str, ran: &Ran, stdout: &str) {
        assert_eq!((ran.code, ran.stderr.as_str()), (Some(0), ""), "{what}: stdout {}", ran.stdout);
        assert_eq!(ran.stdout, stdout, "{what}");
    }

    #[test]
    fn install_adds_exactly_the_open_with_entries_and_leaves_other_programs_untouched() {
        let scratch = Scratch::new("install", true);
        let binary = scratch.fake_binary();
        let registry_before = scratch.registry();
        let files_before = scratch.files();

        let ran = scratch.install(&binary);

        assert_succeeded("install", &ran, &install_stdout(&scratch));
        // Equality with before + expected also proves no value of another program (the .mp4
        // default "VLC.mp4", VLC's OpenWithProgids, ...) changed or went missing.
        let mut expected = registry_before.clone();
        expected.extend(installed_registry(&scratch, None));
        assert_eq!(scratch.registry(), expected);
        assert_eq!(scratch.files(), installed_files(&files_before));
        assert_eq!(std::fs::read(scratch.exe()).unwrap(), std::fs::read(&binary).unwrap(), "installed playr.exe");
        assert_eq!(std::fs::read(scratch.install_dir().join("uninstall.ps1")).unwrap(), std::fs::read(installer()).unwrap(), "uninstall.ps1");
    }

    #[test]
    fn the_start_menu_shortcut_starts_the_installed_exe_with_its_icon() {
        let scratch = Scratch::new("shortcut", false);
        let ran = scratch.install(&scratch.fake_binary());
        assert_succeeded("install", &ran, &install_stdout(&scratch));

        let shortcut = scratch.start_menu().join("playr.lnk");
        let exe = scratch.exe().display().to_string();
        assert_eq!(
            run_helper("shortcut.ps1", &[&shortcut.display().to_string()]),
            format!("{exe}\n{}\n{exe},0\n{DESCRIPTION}\n", scratch.install_dir().display())
        );
    }

    #[test]
    fn the_uninstall_version_is_the_product_version_of_the_installed_binary() {
        let scratch = Scratch::new("version", false);
        let binary = scratch.ping_binary();
        let version = run_helper("version.ps1", &[&binary.display().to_string()]).trim().to_string();
        assert!(!version.is_empty(), "ping.exe has no ProductVersion to compare with");

        let ran = scratch.install(&binary);

        assert_succeeded("install", &ran, &install_stdout(&scratch));
        assert_eq!(scratch.value("Microsoft\\Windows\\CurrentVersion\\Uninstall\\playr", "DisplayVersion"), version);
    }

    #[test]
    fn installing_again_changes_nothing() {
        let scratch = Scratch::new("twice", true);
        let binary = scratch.fake_binary();
        assert_succeeded("first install", &scratch.install(&binary), &install_stdout(&scratch));
        let registry_once = scratch.registry();
        let files_once = scratch.files();

        let ran = scratch.install(&binary);

        assert_succeeded("second install", &ran, &install_stdout(&scratch));
        assert_eq!(scratch.registry(), registry_once);
        assert_eq!(scratch.files(), files_once);
    }

    #[test]
    fn installing_again_from_the_installed_copy_updates_in_place() {
        let scratch = Scratch::new("update", false);
        assert_succeeded("first install", &scratch.install(&scratch.fake_binary()), &install_stdout(&scratch));
        let registry_once = scratch.registry();
        let newer = scratch.dir.join("build").join("playr-newer.exe");
        std::fs::write(&newer, b"MZ a newer stand-in").unwrap();

        // The copied uninstall.ps1 is the installer too; running it must not try to copy itself onto itself.
        let ran = ran(powershell()
            .arg("-File")
            .arg(scratch.install_dir().join("uninstall.ps1"))
            .arg("-Binary")
            .arg(&newer)
            .arg("-InstallDir")
            .arg(scratch.install_dir())
            .arg("-StartMenuDir")
            .arg(scratch.start_menu())
            .arg("-RegistryRoot")
            .arg(scratch.root())
            .output()
            .unwrap());

        assert_succeeded("install from the installed copy", &ran, &install_stdout(&scratch));
        assert_eq!(std::fs::read(scratch.exe()).unwrap(), b"MZ a newer stand-in");
        assert_eq!(scratch.registry(), registry_once);
    }

    #[test]
    fn the_registered_uninstall_command_removes_everything_the_install_added() {
        let scratch = Scratch::new("uninstall", true);
        let binary = scratch.fake_binary();
        let registry_before = scratch.registry();
        let files_before = scratch.files();
        assert_succeeded("install", &scratch.install(&binary), &install_stdout(&scratch));

        let ran = scratch.run_uninstall_string();

        assert_succeeded("UninstallString", &ran, "playr is uninstalled.\n");
        assert_eq!(scratch.registry(), registry_before);
        assert_eq!(scratch.files(), files_before);
    }

    #[test]
    fn uninstall_when_nothing_is_installed_changes_nothing() {
        let scratch = Scratch::new("noop", true);
        let registry_before = scratch.registry();
        let files_before = scratch.files();

        let ran = scratch.uninstall();

        assert_succeeded("uninstall", &ran, "playr is uninstalled.\n");
        assert_eq!(scratch.registry(), registry_before);
        assert_eq!(scratch.files(), files_before);
    }

    #[test]
    fn install_and_uninstall_refuse_while_the_installed_playr_runs() {
        let scratch = Scratch::new("running", true);
        assert_succeeded("install", &scratch.install(&scratch.ping_binary()), &install_stdout(&scratch));
        let registry_installed = scratch.registry();
        let files_installed = scratch.files();
        let exe = scratch.exe().display().to_string();
        let running = Running::start(&scratch.exe());

        let reinstall = scratch.install(&scratch.fake_binary());
        let uninstall = scratch.uninstall();

        assert_eq!(reinstall.code, Some(1), "install while running: stdout {}", reinstall.stdout);
        assert_eq!(error_message(&reinstall.stderr), format!("playr is running from {exe}; close it and run the installer again."));
        assert_eq!(uninstall.code, Some(1), "uninstall while running: stdout {}", uninstall.stdout);
        assert_eq!(error_message(&uninstall.stderr), format!("playr is running from {exe}; close it and run the uninstaller again."));
        assert_eq!(scratch.registry(), registry_installed);
        assert_eq!(scratch.files(), files_installed);
        let ping = std::fs::read(Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("PING.EXE")).unwrap();
        assert!(std::fs::read(scratch.exe()).unwrap() == ping, "the refused install replaced the running playr.exe");

        drop(running);
        assert_succeeded("uninstall after playr exited", &scratch.uninstall(), "playr is uninstalled.\n");
    }

    #[test]
    fn a_playr_running_from_another_folder_does_not_block_the_install() {
        let scratch = Scratch::new("elsewhere", false);
        let other = scratch.dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_exe = other.join("playr.exe");
        std::fs::copy(scratch.ping_binary(), &other_exe).unwrap();
        let _running = Running::start(&other_exe);

        let ran = scratch.install(&scratch.fake_binary());

        assert_succeeded("install", &ran, &install_stdout(&scratch));
    }

    #[test]
    fn a_missing_binary_is_refused_and_nothing_is_installed() {
        let scratch = Scratch::new("missing", true);
        let registry_before = scratch.registry();
        let files_before = scratch.files();
        let missing = scratch.dir.join("build").join("not-built.exe");

        let ran = scratch.install(&missing);

        assert_eq!(ran.code, Some(1), "stdout {}", ran.stdout);
        assert_eq!(error_message(&ran.stderr), format!("No such file: {}", missing.display()));
        assert_eq!(scratch.registry(), registry_before);
        assert_eq!(scratch.files(), files_before);
    }

    /// Locations a user can plausibly pass, each installed and then removed through the
    /// registered UninstallString, which must find every one of them again.
    #[test]
    fn the_registered_uninstall_command_works_for_unusual_locations() {
        let mut failures = Vec::new();
        for (case, install_dir_name, start_menu_suffix, root_suffix) in [
            ("install dir with a trailing backslash", "playr\\", "", ""),
            ("start menu dir with a trailing backslash", "playr", "\\", ""),
            ("registry root with a trailing backslash", "playr", "", "\\"),
            ("all three with two trailing backslashes", "playr\\\\", "\\\\", "\\\\"),
            ("install dir with brackets and a semicolon", "playr [x86];1", "", ""),
        ] {
            let scratch = Scratch::new("odd", true);
            let binary = scratch.fake_binary();
            let registry_before = scratch.registry();
            let files_before = scratch.files();
            let install_dir = scratch.dir.join("Programs O'Neil").join(install_dir_name);
            let start_menu = PathBuf::from(format!("{}{start_menu_suffix}", scratch.start_menu().display()));
            let root = format!("{}{root_suffix}", scratch.root());

            let install = scratch.install_with(&binary, &install_dir, &start_menu, &root);
            if install.code != Some(0) {
                failures.push(format!("{case}: install exited {:?}: {}", install.code, install.stderr));
                continue;
            }
            let uninstall = scratch.run_uninstall_string();
            if uninstall.code != Some(0) || uninstall.stdout != "playr is uninstalled.\n" {
                failures.push(format!("{case}: UninstallString exited {:?}: {}{}", uninstall.code, uninstall.stdout, uninstall.stderr));
            }
            let left: Vec<String> = scratch.registry().difference(&registry_before).cloned().collect();
            if !left.is_empty() {
                failures.push(format!("{case}: registry left behind: {left:?}"));
            }
            let files_left: Vec<String> = scratch.files().difference(&files_before).cloned().collect();
            if !files_left.is_empty() {
                failures.push(format!("{case}: files left behind: {files_left:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn install_cmd_passes_its_arguments_on_to_the_script() {
        let scratch = Scratch::new("cmd", true);
        let registry_before = scratch.registry();
        let cmd = repo().join("install").join("windows").join("install.cmd");
        let binary = scratch.fake_binary();
        let common = |command: &mut Command| {
            command
                .arg("-InstallDir")
                .arg(scratch.install_dir())
                .arg("-StartMenuDir")
                .arg(scratch.start_menu())
                .arg("-RegistryRoot")
                .arg(scratch.root())
                .stdin(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW);
        };

        let mut install = Command::new(&cmd);
        install.arg("-Binary").arg(&binary);
        common(&mut install);
        let installed = ran(install.output().expect("run install.cmd"));
        let registry_installed = scratch.registry();
        let mut uninstall = Command::new(&cmd);
        uninstall.arg("-Uninstall");
        common(&mut uninstall);
        let uninstalled = ran(uninstall.output().expect("run install.cmd -Uninstall"));

        assert_eq!((installed.code, installed.stderr.as_str()), (Some(0), ""), "install.cmd: {}", installed.stdout);
        let mut expected = registry_before.clone();
        expected.extend(installed_registry(&scratch, None));
        assert_eq!(registry_installed, expected);
        assert_eq!((uninstalled.code, uninstalled.stdout.as_str(), uninstalled.stderr.as_str()), (Some(0), "playr is uninstalled.\n", ""));
        assert_eq!(scratch.registry(), registry_before);
    }

    /// -InstallDir may name a folder that holds other files; uninstall takes out only the two
    /// files the install put there. (A folder the install created is removed: see
    /// `the_registered_uninstall_command_removes_everything_the_install_added`.)
    #[test]
    fn uninstall_keeps_other_files_in_the_install_folder() {
        let scratch = Scratch::new("shared-dir", true);
        let binary = scratch.fake_binary();
        std::fs::create_dir_all(scratch.install_dir().join("plugins")).unwrap();
        std::fs::write(scratch.install_dir().join("notes.txt"), b"mine").unwrap();
        std::fs::write(scratch.install_dir().join("plugins").join("readme.txt"), b"also mine").unwrap();
        let registry_before = scratch.registry();
        let files_before = scratch.files();
        assert_succeeded("install", &scratch.install(&binary), &install_stdout(&scratch));

        let ran = scratch.run_uninstall_string();

        assert_succeeded("UninstallString", &ran, "playr is uninstalled.\n");
        assert_eq!(scratch.files(), files_before);
        assert_eq!(std::fs::read(scratch.install_dir().join("notes.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(scratch.install_dir().join("plugins").join("readme.txt")).unwrap(), b"also mine");
        assert_eq!(scratch.registry(), registry_before);
    }

    /// A free drive letter mapped with `subst` to a folder, unmapped on drop.
    struct Subst(char);

    impl Subst {
        fn new(target: &Path) -> Self {
            for letter in ('D'..='Z').rev() {
                if Path::new(&format!("{letter}:\\")).exists() {
                    continue;
                }
                let mapped = Command::new("subst").arg(format!("{letter}:")).arg(target).creation_flags(CREATE_NO_WINDOW).output().expect("run subst");
                if mapped.status.success() {
                    return Subst(letter);
                }
            }
            panic!("no free drive letter for subst");
        }
    }

    impl Drop for Subst {
        fn drop(&mut self) {
            let _ = Command::new("subst").arg(format!("{}:", self.0)).arg("/D").creation_flags(CREATE_NO_WINDOW).output();
        }
    }

    /// A drive root ends in a backslash that the UninstallString must double, or it would escape
    /// the closing quote; uninstall then removes playr's files and keeps the drive's own.
    #[test]
    fn a_drive_root_as_install_and_start_menu_folder_is_uninstalled_by_the_registered_command() {
        let scratch = Scratch::new("drive-root", true);
        let binary = scratch.fake_binary();
        let drive_dir = scratch.dir.join("drive");
        std::fs::create_dir_all(drive_dir.join("keep")).unwrap();
        std::fs::write(drive_dir.join("keep.txt"), b"keep").unwrap();
        std::fs::write(drive_dir.join("keep").join("inner.txt"), b"inner").unwrap();
        let registry_before = scratch.registry();
        let files_before = scratch.files();
        let drive = Subst::new(&drive_dir);
        let root = PathBuf::from(format!("{}:\\", drive.0));

        let install = scratch.install_with(&binary, &root, &root, &scratch.root());
        let installed_files = scratch.files();
        let uninstall_string = scratch.value("Microsoft\\Windows\\CurrentVersion\\Uninstall\\playr", "UninstallString");
        let uninstall = scratch.run_uninstall_string();

        let d = drive.0;
        assert_succeeded(
            "install",
            &install,
            &format!(
                "playr is installed in {d}:\\.\nRight-click a media file > Open with > playr. To make playr the default player,\nchoose it in Settings > Apps > Default apps > playr.\n"
            ),
        );
        let mut expected_installed = files_before.clone();
        expected_installed.extend(["drive\\playr.exe", "drive\\uninstall.ps1", "drive\\playr.lnk"].map(str::to_string));
        assert_eq!(installed_files, expected_installed);
        assert_eq!(
            uninstall_string,
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{d}:\\uninstall.ps1\" -Uninstall -InstallDir \"{d}:\\\\\" -StartMenuDir \"{d}:\\\\\" -RegistryRoot \"{}\"",
                scratch.root()
            )
        );
        assert_succeeded("UninstallString", &uninstall, "playr is uninstalled.\n");
        assert_eq!(scratch.files(), files_before);
        assert_eq!(std::fs::read(drive_dir.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(scratch.registry(), registry_before);
    }

    /// Without -Binary, install.ps1 builds the repository two folders above it with cargo and
    /// installs target\release\playr.exe. A copy of the script in a scratch repository layout
    /// and a stub cargo.cmd stand in for the real ones; PATH holds Windows and the stubs only.
    #[test]
    fn without_binary_the_script_builds_with_cargo_and_installs_the_result() {
        struct Row {
            name: &'static str,
            cargo_toml: bool,
            /// `None`: no cargo on PATH at all.
            cargo_exit: Option<i32>,
            stale_binary: bool,
            code: i32,
            stdout_prefix: &'static str,
            /// Leading stderr lines from cargo itself, before PowerShell's error record.
            cargo_stderr: &'static str,
            /// `{repo}` is replaced by the scratch repository folder.
            error: &'static str,
            cargo_ran: bool,
            installs: bool,
        }
        let building = "Building playr (cargo build --release)...\n";
        let rows = [
            Row {
                name: "cargo builds, the result is installed",
                cargo_toml: true,
                cargo_exit: Some(0),
                stale_binary: false,
                code: 0,
                stdout_prefix: building,
                cargo_stderr: "Compiling fake\n",
                error: "",
                cargo_ran: true,
                installs: true,
            },
            Row {
                name: "no Cargo.toml two folders up",
                cargo_toml: false,
                cargo_exit: Some(0),
                stale_binary: false,
                code: 1,
                stdout_prefix: "",
                cargo_stderr: "",
                error: "No playr source next to this script ({repo}). Pass -Binary <path to playr.exe>.",
                cargo_ran: false,
                installs: false,
            },
            Row {
                name: "no cargo on PATH",
                cargo_toml: true,
                cargo_exit: None,
                stale_binary: false,
                code: 1,
                stdout_prefix: "",
                cargo_stderr: "",
                error: "cargo was not found. Install Rust from https://rustup.rs, or pass -Binary <path to playr.exe>.",
                cargo_ran: false,
                installs: false,
            },
            Row {
                name: "a failed build does not install the stale binary of an earlier one",
                cargo_toml: true,
                cargo_exit: Some(101),
                stale_binary: true,
                code: 1,
                stdout_prefix: building,
                cargo_stderr: "Compiling fake\n",
                error: "cargo build failed (exit code 101)",
                cargo_ran: true,
                installs: false,
            },
        ];
        let system = std::env::var("SystemRoot").expect("SystemRoot");
        let mut failures = Vec::new();
        for row in rows {
            let scratch = Scratch::new("cargo", true);
            let repo = scratch.dir.join("repo");
            let script = repo.join("install").join("windows").join("install.ps1");
            let stubs = scratch.dir.join("stubs");
            let log = scratch.dir.join("cargo.log");
            std::fs::create_dir_all(script.parent().unwrap()).unwrap();
            std::fs::create_dir_all(&stubs).unwrap();
            std::fs::copy(installer(), &script).unwrap();
            if row.cargo_toml {
                std::fs::write(repo.join("Cargo.toml"), b"[package]\nname = \"playr\"\n").unwrap();
            }
            if row.stale_binary {
                std::fs::create_dir_all(repo.join("target").join("release")).unwrap();
                std::fs::write(repo.join("target").join("release").join("playr.exe"), b"stale").unwrap();
            }
            if let Some(exit) = row.cargo_exit {
                std::fs::write(
                    stubs.join("cargo.cmd"),
                    format!(
                        "@echo off\r\n>>\"{}\" echo %* in %CD%\r\n>&2 echo Compiling fake\r\nif not \"{exit}\"==\"0\" exit /b {exit}\r\n\
                         if not exist target\\release mkdir target\\release\r\n>target\\release\\playr.exe echo built-by-stub\r\nexit /b 0\r\n",
                        log.display()
                    ),
                )
                .unwrap();
            }
            let registry_before = scratch.registry();
            let path = format!("{};{system}\\System32;{system};{system}\\System32\\WindowsPowerShell\\v1.0", stubs.display());

            let ran = ran(powershell()
                .env("PATH", &path)
                .arg("-File")
                .arg(&script)
                .arg("-InstallDir")
                .arg(scratch.install_dir())
                .arg("-StartMenuDir")
                .arg(scratch.start_menu())
                .arg("-RegistryRoot")
                .arg(scratch.root())
                .output()
                .expect("start powershell.exe"));

            let mut problems = Vec::new();
            let expected_stdout = if row.installs { format!("{}{}", row.stdout_prefix, install_stdout(&scratch)) } else { row.stdout_prefix.to_string() };
            if ran.code != Some(row.code) || ran.stdout != expected_stdout {
                problems.push(format!("exit {:?}, stdout {:?}; expected exit {}, stdout {expected_stdout:?}", ran.code, ran.stdout, row.code));
            }
            let error = row.error.replace("{repo}", &repo.display().to_string());
            match ran.stderr.strip_prefix(row.cargo_stderr) {
                Some(rest) if error_message(rest) == error => {}
                _ => problems.push(format!("stderr {:?}; expected {:?} then the error {error:?}", ran.stderr, row.cargo_stderr)),
            }
            let logged = std::fs::read_to_string(&log).unwrap_or_default().replace("\r\n", "\n");
            let expected_log = if row.cargo_ran { format!("build --release in {}\n", repo.display()) } else { String::new() };
            if logged != expected_log {
                problems.push(format!("cargo log {logged:?}, expected {expected_log:?}"));
            }
            if row.installs {
                let built = std::fs::read(repo.join("target").join("release").join("playr.exe")).unwrap_or_default();
                if built != b"built-by-stub\r\n" || std::fs::read(scratch.exe()).ok() != Some(built.clone()) {
                    problems.push(format!("installed {:?}, built {built:?}", std::fs::read(scratch.exe()).ok()));
                }
            } else {
                if scratch.install_dir().exists() {
                    problems.push(format!("{} was created", scratch.install_dir().display()));
                }
                if scratch.registry() != registry_before {
                    problems.push("the registry changed".to_string());
                }
            }
            if !problems.is_empty() {
                failures.push(format!("{}: {}", row.name, problems.join("; ")));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

// ---------------------------------------------------------------------------------------------
// Linux: install.sh as an unprivileged user in an ubuntu:24.04 container.

mod linux {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    struct Container {
        name: String,
        /// `docker run -i --rm ... cat`: the container lives while this process holds cat's
        /// stdin open, and is removed when the test process exits and the pipe closes.
        _keep_alive: Mutex<Child>,
    }

    fn container() -> &'static str {
        static CONTAINER: OnceLock<Container> = OnceLock::new();
        &CONTAINER
            .get_or_init(|| {
                let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
                let name = format!("playr-installer-test-{}-{nanos}", std::process::id());
                let mount = format!("{}:/src:ro", repo().display().to_string().replace('\\', "/"));
                let child = Command::new("docker")
                    .args(["run", "-i", "--rm", "--name", &name, "-v", &mount, "ubuntu:24.04", "cat"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("start docker; is Docker installed and running?");
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let state = Command::new("docker").args(["inspect", "-f", "{{.State.Running}}", &name]).output().expect("docker inspect");
                    if String::from_utf8_lossy(&state.stdout).trim() == "true" {
                        break;
                    }
                    assert!(Instant::now() < deadline, "container {name} did not start within 60 s");
                    std::thread::sleep(Duration::from_millis(200));
                }
                let setup = Command::new("docker")
                    .args(["exec", "-e", "DEBIAN_FRONTEND=noninteractive", &name, "bash", "-c"])
                    .arg(
                        "apt-get update -qq && apt-get install -y -qq --no-install-recommends desktop-file-utils shared-mime-info libglib2.0-bin gtk-update-icon-cache >/dev/null \
                         && useradd -m tester",
                    )
                    .output()
                    .expect("docker exec");
                assert!(setup.status.success(), "container setup failed: {}", String::from_utf8_lossy(&setup.stderr));
                Container { name, _keep_alive: Mutex::new(child) }
            })
            .name
    }

    #[derive(Debug, PartialEq)]
    struct Ran {
        code: i32,
        stdout: String,
        stderr: String,
    }

    /// Single-quotes a string for bash.
    fn sq(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    /// One test's world in the container: HOME=/tmp/<name>, a stand-in binary in /tmp/bin-<name>.
    struct Case {
        name: String,
    }

    impl Case {
        fn new(name: &str) -> Self {
            let case = Case { name: name.to_string() };
            let ran = case.run_raw(&format!(
                "rm -rf /tmp/{name} /tmp/bin-{name} && mkdir -p /tmp/{name} /tmp/bin-{name} && \
                 printf '#!/bin/sh\\nprintf \"%%s\\\\n\" \"$0\" \"$@\" > /tmp/bin-{name}/argv\\n' > /tmp/bin-{name}/playr-build && \
                 chmod 755 /tmp/bin-{name}/playr-build"
            ));
            assert_eq!(ran.code, 0, "case setup: {ran:?}");
            case
        }

        fn home(&self) -> String {
            format!("/tmp/{}", self.name)
        }

        fn fake(&self) -> String {
            format!("/tmp/bin-{}/playr-build", self.name)
        }

        fn run_raw(&self, script: &str) -> Ran {
            let out = Command::new("docker")
                .args(["exec", "-u", "tester", "-w", "/tmp", container(), "bash", "-c", script])
                .stdin(Stdio::null())
                .output()
                .expect("docker exec");
            Ran {
                code: out.status.code().expect("docker exec exit code"),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
        }

        /// Runs `script` with HOME set to this case's home and the working directory there.
        fn run(&self, script: &str) -> Ran {
            self.run_raw(&format!("export HOME={home}; cd \"$HOME\"; {script}", home = self.home()))
        }

        fn ok(&self, script: &str) -> String {
            let ran = self.run(script);
            assert_eq!((ran.code, ran.stderr.as_str()), (0, ""), "{script}\nstdout: {}", ran.stdout);
            ran.stdout
        }

        /// Every path under `dir` with its type and mode, and each file's checksum.
        fn tree(&self, dir: &str) -> String {
            self.ok(&format!(
                "cd {dir} && find . -mindepth 1 -printf '%p %y %m\\n' | LC_ALL=C sort && find . -type f -exec md5sum {{}} + | LC_ALL=C sort -k2"
            ))
        }

        /// [`Case::tree`] without the icon theme cache: gtk-update-icon-cache rewrites it on
        /// every install and uninstall, and a regenerated cache left behind is accepted.
        fn tree_without_icon_cache(&self, dir: &str) -> String {
            self.tree(dir).lines().filter(|line| !line.ends_with("/icon-theme.cache") && !line.contains("/icon-theme.cache ")).map(|line| format!("{line}\n")).collect()
        }
    }

    const INSTALL: &str = "bash /src/install/linux/install.sh";

    const MIME_TYPES: &str = "video/mp4;video/x-m4v;video/quicktime;video/x-matroska;video/webm;video/x-msvideo;video/avi;video/vnd.avi;\
audio/mpeg;audio/mp3;audio/flac;audio/x-flac;audio/aac;audio/x-aac;audio/mp4;audio/x-m4a;audio/ogg;audio/x-vorbis+ogg;audio/x-opus+ogg;\
audio/x-flac+ogg;video/ogg;audio/x-wav;audio/wav;audio/vnd.wave;audio/x-mpegurl;audio/mpegurl;application/vnd.apple.mpegurl;\
application/x-mpegurl;audio/x-scpls;";

    fn desktop_entry(exec: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=playr\nGenericName=Media Player\n\
             Comment=Light, fast media player: video files, audio files and internet radio\nExec={exec}\nIcon=playr\nTerminal=false\n\
             Categories=AudioVideo;Audio;Video;Player;\nKeywords=video;audio;music;movie;radio;player;\nMimeType={MIME_TYPES}\n\
             StartupWMClass=playr\n"
        )
    }

    fn installed_stdout(bin_dir: &str, on_path: bool) -> String {
        let mut out = format!("playr is installed: {bin_dir}/playr\nRight-click a media file > Open With > playr.\n");
        if !on_path {
            out += &format!("Note: {bin_dir} is not on PATH, so 'playr' does not run from a terminal (the menu entry works).\n");
        }
        out
    }

    /// The Exec value split into arguments the way the Desktop Entry Specification reads it:
    /// first the string escapes (`\s \n \t \r \\`), then the quoting rules (inside double
    /// quotes a backslash escapes `"` `` ` `` `$` `\`), then `%%` is a literal `%`.
    fn exec_argv(value: &str) -> Vec<String> {
        let mut unescaped = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                unescaped.push(c);
                continue;
            }
            match chars.next() {
                Some('s') => unescaped.push(' '),
                Some('n') => unescaped.push('\n'),
                Some('t') => unescaped.push('\t'),
                Some('r') => unescaped.push('\r'),
                Some('\\') => unescaped.push('\\'),
                other => panic!("invalid string escape \\{other:?} in {value}"),
            }
        }
        let mut args = Vec::new();
        let mut chars = unescaped.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c == ' ' {
                chars.next();
                continue;
            }
            let mut arg = String::new();
            if c == '"' {
                chars.next();
                loop {
                    match chars.next().unwrap_or_else(|| panic!("unterminated quote in {value}")) {
                        '"' => break,
                        '\\' => {
                            let escaped = chars.next().unwrap();
                            assert!(matches!(escaped, '"' | '`' | '$' | '\\'), "\\{escaped} is no quoting escape in {value}");
                            arg.push(escaped);
                        }
                        '`' | '$' => panic!("unescaped reserved character inside quotes in {value}"),
                        other => arg.push(other),
                    }
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ' ' {
                        break;
                    }
                    arg.push(c);
                    chars.next();
                }
            }
            args.push(arg.replace("%%", "%"));
        }
        args
    }

    #[test]
    fn exec_argv_reads_the_specification_examples() {
        assert_eq!(exec_argv(r#""/a b/c" %f"#), ["/a b/c", "%f"]);
        assert_eq!(exec_argv(r#""/x\\"y\\`z\\$w\\\\v" %f"#), ["/x\"y`z$w\\v", "%f"]);
        assert_eq!(exec_argv(r#"/p/50%%x %u"#), ["/p/50%x", "%u"]);
    }

    fn exec_line(desktop: &str) -> &str {
        desktop.lines().find_map(|line| line.strip_prefix("Exec=")).expect("an Exec= line")
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn installs_binary_icons_and_desktop_entry_under_the_default_per_user_folders() {
        let case = Case::new("defaults");
        let home = case.home();

        let ran = case.run(&format!("{INSTALL} --binary {}", case.fake()));

        assert_eq!(ran, Ran { code: 0, stdout: installed_stdout(&format!("{home}/.local/bin"), false), stderr: String::new() });
        let listing = case.ok("find . -mindepth 1 -printf '%p %y %m\\n' | LC_ALL=C sort");
        assert_eq!(
            listing,
            "./.local d 755\n./.local/bin d 755\n./.local/bin/playr f 755\n./.local/share d 755\n./.local/share/applications d 755\n\
             ./.local/share/applications/mimeinfo.cache f 644\n./.local/share/applications/playr.desktop f 644\n./.local/share/icons d 755\n\
             ./.local/share/icons/hicolor d 755\n./.local/share/icons/hicolor/256x256 d 755\n./.local/share/icons/hicolor/256x256/apps d 755\n\
             ./.local/share/icons/hicolor/256x256/apps/playr.png f 644\n./.local/share/icons/hicolor/icon-theme.cache f 644\n\
             ./.local/share/icons/hicolor/scalable d 755\n\
             ./.local/share/icons/hicolor/scalable/apps d 755\n./.local/share/icons/hicolor/scalable/apps/playr.svg f 644\n"
        );
        assert_eq!(
            case.ok(&format!(
                "cmp {} .local/bin/playr && cmp /src/assets/playr.png .local/share/icons/hicolor/256x256/apps/playr.png && \
                 cmp /src/assets/playr.svg .local/share/icons/hicolor/scalable/apps/playr.svg && echo same",
                case.fake()
            )),
            "same\n"
        );
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn the_desktop_entry_is_valid_and_lists_every_media_type() {
        let case = Case::new("entry");
        case.ok(&format!("{INSTALL} --binary {} >/dev/null", case.fake()));

        let desktop = case.ok("cat .local/share/applications/playr.desktop");
        let validated = case.run("desktop-file-validate .local/share/applications/playr.desktop");

        assert_eq!(desktop, desktop_entry(&format!("\"{}/.local/bin/playr\" %f", case.home())));
        assert_eq!(validated, Ran { code: 0, stdout: String::new(), stderr: String::new() });
    }

    /// Every extension maps to at least one type that shared-mime-info gives files of that
    /// extension (file managers match "Open With" against types, not extensions).
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn every_extension_maps_to_a_type_shared_mime_info_gives_its_files() {
        let case = Case::new("globs");
        let globs = case.ok("cat /usr/share/mime/globs2");
        let aliases = case.ok("cat /usr/share/mime/aliases");
        // An alias (video/x-m4v for video/mp4) names the same type; desktops resolve it.
        let canonical = |mime: &str| -> String {
            aliases.lines().find_map(|line| line.strip_prefix(&format!("{mime} "))).unwrap_or(mime).to_string()
        };
        let entries = linux_script_entries(&read_repo_file("install/linux/install.sh"));

        let mut unmatched = Vec::new();
        for (ext, types) in &entries {
            let pattern = format!("*.{ext}");
            let database: Vec<&str> = globs
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let (_weight, mime, glob) = (fields.next()?, fields.next()?, fields.next()?);
                    (glob == pattern).then_some(mime)
                })
                .collect();
            if !types.iter().any(|mime| database.contains(&canonical(mime).as_str())) {
                unmatched.push(format!(".{ext}: script lists {types:?}, shared-mime-info gives {database:?}"));
            }
        }
        assert_eq!(entries.len(), MEDIA_EXTENSIONS.len());
        assert!(unmatched.is_empty(), "{}", unmatched.join("\n"));
    }

    /// GIO (what GNOME's file chooser and Nautilus use) lists playr for every type the
    /// installed desktop entry declares.
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn gio_offers_playr_for_every_type_in_the_desktop_entry() {
        let case = Case::new("gio-mime");
        case.ok(&format!("{INSTALL} --binary {} >/dev/null", case.fake()));
        let desktop = case.ok("cat .local/share/applications/playr.desktop");
        let types: Vec<&str> = desktop
            .lines()
            .find_map(|line| line.strip_prefix("MimeType="))
            .expect("a MimeType= line")
            .split(';')
            .filter(|mime| !mime.is_empty())
            .collect();
        assert_eq!(types.len(), MIME_TYPES.matches(';').count(), "types in {desktop}");

        let mut missing = Vec::new();
        for mime in types {
            let listed = case.ok(&format!("gio mime {}", sq(mime)));
            let registered = listed
                .split_once("Registered applications:\n")
                .map(|(_, rest)| rest.lines().take_while(|line| line.starts_with('\t')).any(|line| line == "\tplayr.desktop"))
                .unwrap_or(false);
            if !registered {
                missing.push(format!("{mime}: {}", listed.trim()));
            }
        }
        assert!(missing.is_empty(), "playr is not offered for:\n{}", missing.join("\n"));
    }

    /// A bin folder with every character the Exec quoting has to handle except `%`: the desktop
    /// entry stays valid, and GIO starts exactly that path with the file as one argument.
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn an_odd_bin_folder_is_quoted_so_that_gio_launches_that_exact_path() {
        let case = Case::new("odd");
        let home = case.home();
        let bin_dir = format!("{home}/o d\"q`x$y\\z'v");
        let data_dir = format!("{home}/data dir");
        let env = format!("export PLAYR_BIN_DIR={} XDG_DATA_HOME={} PATH={}:\"$PATH\";", sq(&bin_dir), sq(&data_dir), sq(&bin_dir));

        let ran = case.run(&format!("{env} {INSTALL} --binary {}", case.fake()));
        let desktop = case.ok(&format!("{env} cat \"$XDG_DATA_HOME/applications/playr.desktop\""));
        let validated = case.run(&format!("{env} desktop-file-validate \"$XDG_DATA_HOME/applications/playr.desktop\""));
        let launched = case.ok(&format!(
            "{env} gio launch \"$XDG_DATA_HOME/applications/playr.desktop\" \"$HOME/a b.mp4\" && \
             for i in $(seq 100); do [ -s /tmp/bin-odd/argv ] && break; sleep 0.1; done; cat /tmp/bin-odd/argv"
        ));

        assert_eq!(ran, Ran { code: 0, stdout: installed_stdout(&bin_dir, true), stderr: String::new() });
        // Derived by hand from the spec: quote escapes \" \` \$ \\, then every \ doubled again.
        assert_eq!(exec_line(&desktop), format!("\"{home}/o d\\\\\"q\\\\`x\\\\$y\\\\\\\\z'v/playr\" %f"));
        assert_eq!(exec_argv(exec_line(&desktop)), [format!("{bin_dir}/playr"), "%f".to_string()]);
        assert_eq!(validated, Ran { code: 0, stdout: String::new(), stderr: String::new() });
        assert_eq!(launched, format!("{bin_dir}/playr\n{home}/a b.mp4\n"));
    }

    /// `%` is doubled in Exec. GLib cannot load such an entry (it looks for the program under the
    /// still-escaped name), so this checks the file against the specification only.
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn a_percent_sign_in_the_bin_folder_is_escaped_as_the_specification_says() {
        let case = Case::new("percent");
        let bin_dir = format!("{}/50% off", case.home());
        let env = format!("export PLAYR_BIN_DIR={};", sq(&bin_dir));

        let ran = case.run(&format!("{env} {INSTALL} --binary {}", case.fake()));
        let desktop = case.ok("cat .local/share/applications/playr.desktop");
        let validated = case.run("desktop-file-validate .local/share/applications/playr.desktop");

        assert_eq!(ran, Ran { code: 0, stdout: installed_stdout(&bin_dir, false), stderr: String::new() });
        assert_eq!(exec_line(&desktop), format!("\"{}/50%% off/playr\" %f", case.home()));
        assert_eq!(exec_argv(exec_line(&desktop)), [format!("{bin_dir}/playr"), "%f".to_string()]);
        assert_eq!(validated, Ran { code: 0, stdout: String::new(), stderr: String::new() });
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn installing_again_changes_nothing() {
        let case = Case::new("twice");
        let install = format!("{INSTALL} --binary {}", case.fake());
        let first = case.run(&install);
        let tree_once = case.tree("$HOME");

        let second = case.run(&install);

        assert_eq!(first, Ran { code: 0, stdout: installed_stdout(&format!("{}/.local/bin", case.home()), false), stderr: String::new() });
        assert_eq!(second, first);
        assert_eq!(case.tree("$HOME"), tree_once);
    }

    /// The new binary is copied to a temporary name and renamed, so a running playr does not
    /// make the update fail with "Text file busy".
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn installing_over_a_running_playr_replaces_the_binary() {
        let case = Case::new("busy");
        case.ok(&format!("cp /bin/sleep /tmp/bin-busy/sleeper && {INSTALL} --binary /tmp/bin-busy/sleeper >/dev/null"));

        let ran = case.run(&format!(
            ".local/bin/playr 300 & sleeping=$!; {INSTALL} --binary {fake}; code=$?; \
             kill -0 $sleeping && echo still-running; kill $sleeping; cmp {fake} .local/bin/playr && echo replaced; exit $code",
            fake = case.fake()
        ));

        assert_eq!(
            ran,
            Ran {
                code: 0,
                stdout: format!("{}still-running\nreplaced\n", installed_stdout(&format!("{}/.local/bin", case.home()), false)),
                stderr: String::new()
            }
        );
    }

    /// A home where other programs already have a desktop entry, icons and a binary, and the
    /// MIME cache is built: install + uninstall leaves it byte for byte as it was.
    fn lived_in_home(case: &Case) -> String {
        case.ok(
            "mkdir -p .local/bin .local/share/applications .local/share/icons/hicolor/256x256/apps .local/share/icons/hicolor/scalable/apps && \
             printf '#!/bin/sh\\n' > .local/bin/other && chmod 755 .local/bin/other && \
             printf '[Desktop Entry]\\nType=Application\\nName=Other\\nExec=other %%f\\nMimeType=video/mp4;audio/ogg;\\n' > .local/share/applications/other.desktop && \
             printf 'png' > .local/share/icons/hicolor/256x256/apps/other.png && printf '<svg/>' > .local/share/icons/hicolor/scalable/apps/other.svg && \
             update-desktop-database .local/share/applications",
        );
        case.tree_without_icon_cache("$HOME")
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn uninstall_removes_everything_the_install_added_and_nothing_else() {
        let case = Case::new("uninstall");
        let before = lived_in_home(&case);
        case.ok(&format!("{INSTALL} --binary {} >/dev/null", case.fake()));

        let ran = case.run(&format!("{INSTALL} --uninstall"));

        assert_eq!(ran, Ran { code: 0, stdout: "playr is uninstalled.\n".to_string(), stderr: String::new() });
        assert_eq!(case.tree_without_icon_cache("$HOME"), before);
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn uninstall_from_custom_folders_leaves_no_playr_file() {
        let case = Case::new("uninstall-custom");
        let env = "export PLAYR_BIN_DIR=\"$HOME/my bin\" XDG_DATA_HOME=\"$HOME/my data\";";
        case.ok(&format!("{env} {INSTALL} --binary {} >/dev/null", case.fake()));

        let ran = case.run(&format!("{env} {INSTALL} --uninstall"));
        let left = case.run("find . -iname '*playr*'; grep -rl playr . ; true");

        assert_eq!(ran, Ran { code: 0, stdout: "playr is uninstalled.\n".to_string(), stderr: String::new() });
        assert_eq!(left, Ran { code: 0, stdout: String::new(), stderr: String::new() });
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn uninstall_when_nothing_is_installed_changes_nothing() {
        let case = Case::new("uninstall-noop");
        let before = lived_in_home(&case);

        let ran = case.run(&format!("{INSTALL} --uninstall"));

        assert_eq!(ran, Ran { code: 0, stdout: "playr is uninstalled.\n".to_string(), stderr: String::new() });
        assert_eq!(case.tree_without_icon_cache("$HOME"), before);
    }

    /// `dpkg-query` and `sudo` are replaced by stubs: dpkg-query reports the packages in
    /// $INSTALLED as installed, sudo logs its arguments. The container has no cargo, so a
    /// build that gets past the package check stops at "cargo was not found".
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn missing_build_packages_are_reported_with_the_apt_command() {
        let cargo_missing = "install.sh: cargo was not found; install Rust from https://rustup.rs, or pass --binary PATH\n";
        let needs = |packages: &str| {
            format!(
                "install.sh: the build needs these packages: {packages}\n  install them with:  sudo apt-get install {packages}\n  or run this script with --install-deps\n"
            )
        };
        let rows: [(&str, &str, &str, i32, String, String, &str); 6] = [
            ("none installed", "", "", 1, String::new(), needs("build-essential pkg-config libasound2-dev"), ""),
            ("only pkg-config installed", "pkg-config", "", 1, String::new(), needs("build-essential libasound2-dev"), ""),
            ("all installed", "build-essential pkg-config libasound2-dev", "", 1, String::new(), cargo_missing.to_string(), ""),
            (
                "--install-deps with two missing",
                "pkg-config",
                "--install-deps",
                1,
                String::new(),
                format!("Installing build packages: build-essential libasound2-dev\n{cargo_missing}"),
                "apt-get install -y build-essential libasound2-dev\n",
            ),
            ("--install-deps with none missing", "build-essential pkg-config libasound2-dev", "--install-deps", 1, String::new(), cargo_missing.to_string(), ""),
            ("--binary skips the package check", "", "--binary /tmp/bin-deps/playr-build", 0, installed_stdout("/tmp/deps/.local/bin", false), String::new(), ""),
        ];
        let case = Case::new("deps");
        let mut failures = Vec::new();
        for (row, installed, args, code, stdout, stderr, sudo_log) in rows {
            let ran = case.run(&format!(
                "rm -rf \"$HOME\"/* \"$HOME\"/.[!.]* /tmp/stubs-deps; mkdir -p /tmp/stubs-deps && \
                 printf '#!/bin/sh\\ncase \" $INSTALLED \" in *\" $3 \"*) printf \"install ok installed\" ;; *) exit 1 ;; esac\\n' > /tmp/stubs-deps/dpkg-query && \
                 printf '#!/bin/sh\\nprintf \"%%s\\\\n\" \"$*\" >> /tmp/stubs-deps/sudo.log\\n' > /tmp/stubs-deps/sudo && chmod 755 /tmp/stubs-deps/* && \
                 INSTALLED={} PATH=/tmp/stubs-deps:$PATH {INSTALL} {args}; code=$?; cat /tmp/stubs-deps/sudo.log 2>/dev/null | sed 's/^/sudo: /'; exit $code",
                sq(installed)
            ));
            let logged: String = sudo_log.lines().map(|line| format!("sudo: {line}\n")).collect();
            let expected = Ran { code, stdout: format!("{stdout}{logged}"), stderr };
            if ran != expected {
                failures.push(format!("{row}:\n  expected {expected:?}\n  actual   {ran:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Without --binary the script builds with cargo and installs target/release/playr. A stub
    /// cargo, in a copy of the repository, stands in for the real build. Stub cargo and stub
    /// sudo (standing in for `sudo apt-get`) print to stdout as the real ones do; none of that
    /// may end up in the binary path the script captures from its build step, so it is
    /// expected on stderr together with the script's other progress lines.
    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn without_binary_the_cargo_build_result_is_installed() {
        struct Row {
            name: &'static str,
            /// Packages the stub dpkg-query reports installed.
            installed: &'static str,
            args: &'static str,
            /// Where the stub cargo goes; /tmp/stubs-build is on PATH.
            cargo_dir: &'static str,
            cargo_exit: i32,
            sudo_exit: i32,
            cargo_toml: bool,
            /// A target/release/playr left over from an earlier build.
            stale_binary: bool,
            code: i32,
            stdout: String,
            stderr: String,
            sudo_log: &'static str,
            cargo_log: &'static str,
            /// Content of the installed ~/.local/bin/playr, if one must be there.
            installed_binary: Option<&'static str>,
        }
        let all = "build-essential pkg-config libasound2-dev";
        let built = "Building playr (cargo build --release)...\nCompiling fake\n";
        let ok = installed_stdout("/tmp/build/.local/bin", false);
        let apt = "apt-get install -y build-essential libasound2-dev\n";
        let cargo_ran = "build --release in /tmp/repo-build\n";
        let base = || Row {
            name: "",
            installed: all,
            args: "",
            cargo_dir: "/tmp/stubs-build",
            cargo_exit: 0,
            sudo_exit: 0,
            cargo_toml: true,
            stale_binary: false,
            code: 0,
            stdout: ok.clone(),
            stderr: built.to_string(),
            sudo_log: "",
            cargo_log: cargo_ran,
            installed_binary: Some("built-by-stub"),
        };
        let rows = [
            Row { name: "cargo on PATH", ..base() },
            Row { name: "cargo only in ~/.cargo/bin", cargo_dir: "/tmp/build/.cargo/bin", ..base() },
            Row {
                name: "--install-deps installs the missing packages, then builds",
                installed: "pkg-config",
                args: "--install-deps",
                stderr: format!("Installing build packages: build-essential libasound2-dev\nReading package lists... Done\n{built}"),
                sudo_log: apt,
                ..base()
            },
            Row {
                name: "a failed apt-get stops before the build",
                installed: "pkg-config",
                args: "--install-deps",
                sudo_exit: 100,
                code: 1,
                stdout: String::new(),
                stderr: "Installing build packages: build-essential libasound2-dev\nReading package lists... Done\ninstall.sh: apt-get install failed\n".to_string(),
                sudo_log: apt,
                cargo_log: "",
                installed_binary: None,
                ..base()
            },
            Row {
                name: "a failed build does not install the stale binary of an earlier one",
                cargo_exit: 101,
                stale_binary: true,
                code: 1,
                stdout: String::new(),
                stderr: format!("{built}install.sh: cargo build failed\n"),
                installed_binary: None,
                ..base()
            },
            Row {
                name: "no Cargo.toml next to the script",
                cargo_toml: false,
                code: 1,
                stdout: String::new(),
                stderr: "install.sh: no playr source in /tmp/repo-build; pass --binary PATH\n".to_string(),
                cargo_log: "",
                installed_binary: None,
                ..base()
            },
        ];
        let case = Case::new("build");
        let mut failures = Vec::new();
        for row in rows {
            let ran = case.run(&format!(
                "rm -rf \"$HOME\"/* \"$HOME\"/.[!.]* /tmp/stubs-build /tmp/repo-build; \
                 mkdir -p /tmp/stubs-build {cargo_dir} /tmp/repo-build/install/linux /tmp/repo-build/assets && \
                 cp /src/install/linux/install.sh /tmp/repo-build/install/linux/ && cp /src/assets/playr.png /src/assets/playr.svg /tmp/repo-build/assets/ && \
                 {touch} {stale} \
                 printf '#!/bin/sh\\ncase \" $INSTALLED \" in *\" $3 \"*) printf \"install ok installed\" ;; *) exit 1 ;; esac\\n' > /tmp/stubs-build/dpkg-query && \
                 printf '#!/bin/sh\\nprintf \"%%s\\\\n\" \"$*\" >> /tmp/stubs-build/sudo.log\\necho \"Reading package lists... Done\"\\nexit {sudo_exit}\\n' > /tmp/stubs-build/sudo && \
                 printf '#!/bin/sh\\necho \"$* in $(pwd)\" >> /tmp/bin-build/cargo.log\\necho Compiling fake\\n[ {cargo_exit} = 0 ] || exit {cargo_exit}\\nmkdir -p target/release && echo built-by-stub > target/release/playr\\n' > {cargo_dir}/cargo && \
                 chmod 755 /tmp/stubs-build/* {cargo_dir}/cargo && rm -f /tmp/bin-build/cargo.log && \
                 INSTALLED={installed} PATH=/tmp/stubs-build:$PATH bash /tmp/repo-build/install/linux/install.sh {args}; code=$?; \
                 cat /tmp/stubs-build/sudo.log 2>/dev/null | sed 's/^/sudo: /'; \
                 cat /tmp/bin-build/cargo.log 2>/dev/null | sed 's/^/cargo: /'; \
                 [ -f .local/bin/playr ] && sed 's/^/installed: /' .local/bin/playr; exit $code",
                cargo_dir = row.cargo_dir,
                touch = if row.cargo_toml { "touch /tmp/repo-build/Cargo.toml &&" } else { "" },
                stale = if row.stale_binary { "mkdir -p /tmp/repo-build/target/release && echo stale > /tmp/repo-build/target/release/playr &&" } else { "" },
                sudo_exit = row.sudo_exit,
                cargo_exit = row.cargo_exit,
                installed = sq(row.installed),
                args = row.args,
            ));
            let sudo: String = row.sudo_log.lines().map(|line| format!("sudo: {line}\n")).collect();
            let logged: String = row.cargo_log.lines().map(|line| format!("cargo: {line}\n")).collect();
            let installed = row.installed_binary.map(|content| format!("installed: {content}\n")).unwrap_or_default();
            let expected = Ran { code: row.code, stdout: format!("{}{sudo}{logged}{installed}", row.stdout), stderr: row.stderr };
            if ran != expected {
                failures.push(format!("{}:\n  expected {expected:?}\n  actual   {ran:?}", row.name));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    #[ignore = "needs Docker, the ubuntu:24.04 image and network for apt-get"]
    fn options_are_parsed_and_bad_ones_refused_before_anything_is_written() {
        let usage = "Installs playr for the current user (no sudo) and adds it to \"Open With\" for media files.\n\n\
                     \x20 install/linux/install.sh                  build with cargo and install\n\
                     \x20 install/linux/install.sh --binary PATH    install a ready playr binary instead\n\
                     \x20 install/linux/install.sh --install-deps   first apt-get the packages the build needs (sudo)\n\
                     \x20 install/linux/install.sh --uninstall      remove everything an install added\n\n";
        let case = Case::new("options");
        let fake = case.fake();
        let mut failures = Vec::new();
        for (row, args, code, stdout, stderr, files) in [
            ("--help", "--help".to_string(), 0, usage.to_string(), "", ""),
            ("-h", "-h".to_string(), 0, usage.to_string(), "", ""),
            ("unknown option", "--bogus".to_string(), 1, String::new(), "install.sh: unknown option: --bogus (see --help)\n", ""),
            ("--binary without a path", "--binary".to_string(), 1, String::new(), "install.sh: --binary needs a path\n", ""),
            ("--binary of a missing file", "--binary /tmp/options/nothing-here".to_string(), 1, String::new(), "install.sh: no such file: /tmp/options/nothing-here\n", ""),
            ("--binary=PATH", format!("--binary={fake}"), 0, installed_stdout("/tmp/options/.local/bin", false), "", "./.local/bin/playr\n"),
            ("--uninstall wins over --binary", format!("--binary {fake} --uninstall"), 0, "playr is uninstalled.\n".to_string(), "", ""),
        ] {
            let ran = case.run(&format!(
                "rm -rf \"$HOME\"/* \"$HOME\"/.[!.]*; {INSTALL} {args}; code=$?; find . -name playr -type f | sed 's/^/file: /'; exit $code"
            ));
            let listed: String = files.lines().map(|line| format!("file: {line}\n")).collect();
            let expected = Ran { code, stdout: format!("{stdout}{listed}"), stderr: stderr.to_string() };
            if ran != expected {
                failures.push(format!("{row}:\n  expected {expected:?}\n  actual   {ran:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
