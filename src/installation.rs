use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const BASE: &str = "/data/opt/spotify-connect";
pub const ASSET: &str = "spotiya-armv7-linux-gnueabihf.tar.gz";
const FILES: &[&str] = &[
    "spotiya",
    "run.sh",
    "start.sh",
    "stop.sh",
    "boot.sh",
    "service.sh",
    "cli.sh",
    "profiles/mini1.json",
    "profiles/mini2.json",
    "profiles/generic.json",
    "S99spotify-connect",
];
const KEEP: usize = 2;
pub fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
pub fn command(program: &Path, args: &[&OsStr], timeout: Duration) -> io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error("Command timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
pub fn version(binary: &Path) -> io::Result<String> {
    let output = command(binary, &[OsStr::new("--version")], Duration::from_secs(5))?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| error("Invalid binary version"))?;
    let mut words = text.split_whitespace();
    if !output.status.success() || words.next() != Some("spotiya") {
        return Err(error("Binary version check failed"));
    }
    let value = words
        .next()
        .ok_or_else(|| error("Missing binary version"))?;
    crate::update::parse_version(value)?;
    Ok(value.into())
}
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    u64::try_from(u128::from(stat.f_bavail) * u128::from(stat.f_frsize))
        .map_err(|_| error("Filesystem capacity exceeds supported range"))
}
pub fn live_pid(base: &Path, pidfile: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(pidfile).ok()?.trim().parse().ok()?;
    let expected = fs::canonicalize(base.join("spotiya")).ok()?;
    (fs::read_link(format!("/proc/{pid}/exe")).ok()? == expected).then_some(pid)
}
pub struct Temporary(pub PathBuf);
impl Temporary {
    pub fn new(parent: &Path, prefix: &str) -> io::Result<Self> {
        let path = parent.join(format!(".{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct RootMount(bool);
impl RootMount {
    fn writable() -> io::Result<Self> {
        let mounts = fs::read_to_string("/proc/mounts")?;
        let readonly = mounts.lines().any(|l| {
            let f: Vec<_> = l.split_whitespace().collect();
            f.len() > 3 && f[1] == "/" && f[3].split(',').any(|s| s == "ro")
        });
        if readonly {
            Self::remount("remount,rw")?;
        }
        Ok(Self(readonly))
    }
    fn remount(options: &str) -> io::Result<()> {
        let result = command(
            Path::new("/bin/mount"),
            &[OsStr::new("-o"), OsStr::new(options), OsStr::new("/")],
            Duration::from_secs(5),
        )?;
        if result.status.success() {
            Ok(())
        } else {
            Err(error("Cannot remount root filesystem"))
        }
    }
    fn restore(&mut self) -> io::Result<()> {
        if self.0 {
            Self::remount("remount,ro")?;
            self.0 = false;
        }
        Ok(())
    }
}
impl Drop for RootMount {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
#[derive(Clone)]
struct Layout {
    base: PathBuf,
    init: PathBuf,
    cli: PathBuf,
}
impl Layout {
    fn system() -> Self {
        Self {
            base: BASE.into(),
            init: "/etc/init.d/S99spotify-connect".into(),
            cli: "/usr/bin/spotiya".into(),
        }
    }
    fn destination(&self, file: &str) -> PathBuf {
        if file == "S99spotify-connect" {
            self.init.clone()
        } else {
            self.base.join(file)
        }
    }
}
trait Service {
    fn stop(&mut self) -> io::Result<()>;
    fn start(&mut self) -> io::Result<()>;
    fn ready(&mut self, profile: &str) -> io::Result<()>;
}
struct SystemService;
impl Service for SystemService {
    fn stop(&mut self) -> io::Result<()> {
        service("stop")
    }
    fn start(&mut self) -> io::Result<()> {
        service("start")
    }
    fn ready(&mut self, profile: &str) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready_since = None;
        while Instant::now() < deadline {
            let pid = live_pid(Path::new(BASE), Path::new("/run/spotify-connect.pid"));
            let status = fs::read(crate::control::STATUS)
                .ok()
                .and_then(|b| serde_json::from_slice::<crate::control::Status>(&b).ok());
            if pid.is_some()
                && status.is_some_and(|s| Some(s.receiver_pid) == pid && s.profile == profile)
            {
                if ready_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(1)
                {
                    return Ok(());
                }
            } else {
                ready_since = None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(error("Receiver did not become ready after installation"))
    }
}
fn service(action: &str) -> io::Result<()> {
    let result = command(
        Path::new("/etc/init.d/S99spotify-connect"),
        &[OsStr::new(action)],
        Duration::from_secs(12),
    )?;
    if result.status.success() {
        Ok(())
    } else {
        Err(error(format!("Receiver {action} failed")))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: Vec<String>,
    cli_existed: bool,
}
fn copy_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| error("Invalid install destination"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".spotiya-copy-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        fs::copy(source, &temporary)?;
        fs::File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn source(root: &Path, binary: &Path, file: &str) -> PathBuf {
    if file == "spotiya" {
        binary.into()
    } else if file.starts_with("profiles/") {
        root.join(file)
    } else {
        root.join("packaging").join(file)
    }
}
fn snapshot(layout: &Layout) -> io::Result<PathBuf> {
    let parent = layout.base.join("backups");
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!(
        "managed-{:020}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let mut manifest = Manifest {
        version: 1,
        files: vec![],
        cli_existed: fs::symlink_metadata(&layout.cli).is_ok(),
    };
    let result = (|| {
        for &file in FILES {
            let from = layout.destination(file);
            if from.exists() {
                copy_atomic(&from, &path.join(file))?;
                manifest.files.push(file.into());
            }
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("manifest.json"))?;
        f.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        f.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&path);
    }
    result.map(|()| path)
}
fn restore(layout: &Layout, backup: &Path) -> io::Result<()> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(backup.join("manifest.json"))?)?;
    if manifest.version != 1 || manifest.files.iter().any(|f| !FILES.contains(&f.as_str())) {
        return Err(error("Invalid backup manifest"));
    }
    for &file in FILES {
        let destination = layout.destination(file);
        if manifest.files.iter().any(|f| f == file) {
            copy_atomic(&backup.join(file), &destination)?;
        } else if destination.exists() {
            fs::remove_file(destination)?;
        }
    }
    if !manifest.cli_existed && fs::symlink_metadata(&layout.cli).is_ok() {
        fs::remove_file(&layout.cli)?;
    }
    Ok(())
}
fn prune(base: &Path, keep: usize) -> io::Result<()> {
    let mut backups = vec![];
    let entries = match fs::read_dir(base.join("backups")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for item in entries {
        let item = item?;
        if !item.file_name().to_string_lossy().starts_with("managed-")
            || !item.file_type()?.is_dir()
        {
            continue;
        }
        let manifest = fs::read(item.path().join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok());
        if manifest.is_some_and(|m| m.version == 1) {
            backups.push(item.path());
        }
    }
    backups.sort();
    let remove = backups.len().saturating_sub(keep);
    for path in backups.into_iter().take(remove) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}
fn transaction(
    layout: &Layout,
    root: &Path,
    binary: &Path,
    running: bool,
    profile: &str,
    service: &mut impl Service,
) -> io::Result<PathBuf> {
    let (new_bytes, old_bytes) =
        FILES
            .iter()
            .try_fold((0u64, 0u64), |(new, old), &f| -> io::Result<_> {
                let from = source(root, binary, f);
                if !fs::symlink_metadata(&from)?.is_file() {
                    return Err(error("Package contains a non-regular file"));
                }
                Ok((
                    new + fs::metadata(from)?.len(),
                    old + fs::metadata(layout.destination(f)).map_or(0, |m| m.len()),
                ))
            })?;
    // Leave room for the new snapshot while retaining one completed older backup.
    prune(&layout.base, KEEP - 1)?;
    let needed = new_bytes + old_bytes + new_bytes.max(old_bytes) + 2 * 1024 * 1024;
    if free_bytes(&layout.base)? < needed {
        return Err(error(
            "Not enough disk space for staging and rollback backup",
        ));
    }
    let stage = Temporary::new(&layout.base, "install")?;
    for &file in FILES {
        let from = source(root, binary, file);
        if !fs::symlink_metadata(&from)?.is_file() {
            return Err(error("Package contains a non-regular file"));
        }
        copy_atomic(&from, &stage.0.join(file))?;
        fs::set_permissions(
            stage.0.join(file),
            fs::Permissions::from_mode(if file.starts_with("profiles/") {
                0o644
            } else {
                0o755
            }),
        )?;
    }
    let backup = snapshot(layout)?;
    let had_service = layout.init.exists();
    if running && had_service {
        service.stop()?;
    }
    let result: io::Result<()> = (|| {
        for &file in FILES {
            if file == "S99spotify-connect" {
                copy_atomic(&stage.0.join(file), &layout.destination(file))?;
            } else {
                if let Some(parent) = layout.destination(file).parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(stage.0.join(file), layout.destination(file))?;
            }
        }
        if fs::symlink_metadata(&layout.cli).is_err() {
            symlink(layout.base.join("cli.sh"), &layout.cli)?;
        }
        if running {
            service.start()?;
            service.ready(profile)?;
        }
        Ok(())
    })();
    if let Err(cause) = result {
        let _ = service.stop();
        let recovered = restore(layout, &backup).and_then(|()| {
            if running && had_service {
                service.start()?;
                service.ready(profile)?;
            }
            Ok(())
        });
        return Err(error(format!(
            "Installation failed: {cause}. Rollback {}. Backup: {}",
            if recovered.is_ok() {
                "succeeded"
            } else {
                "needs attention"
            },
            backup.display()
        )));
    }
    if let Err(e) = prune(&layout.base, KEEP) {
        eprintln!("Backup cleanup failed: {e}");
    }
    Ok(backup)
}
pub fn install(root: &Path, binary: &Path, newer_only: bool) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("Run the installer as root on the speaker"));
    }
    let layout = Layout::system();
    let state = layout.base.join("receiver-state");
    let _lock = crate::file_lock(&state, "install.lock")?;
    let _selection_lock = crate::file_lock(&state, "profile-selection.lock")?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    if fs::symlink_metadata(&layout.cli).is_ok()
        && fs::read_link(&layout.cli)
            .ok()
            .is_none_or(|p| p != layout.base.join("cli.sh"))
    {
        return Err(error("/usr/bin/spotiya belongs to another installation"));
    }
    let version = version(binary)?;
    if newer_only
        && let Ok(installed) = self::version(&layout.base.join("spotiya"))
        && crate::update::parse_version(&installed)? >= crate::update::parse_version(&version)?
    {
        println!("An equal or newer version is already installed; installation skipped.");
        return Ok(());
    }
    let result = command(
        binary,
        &[
            OsStr::new("--state-dir"),
            state.as_os_str(),
            OsStr::new("--profile-info"),
        ],
        Duration::from_secs(5),
    )?;
    if !result.status.success() {
        return Err(error("Candidate rejected the saved device profile"));
    }
    let profile: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    let profile = profile["profile"]["id"]
        .as_str()
        .ok_or_else(|| error("Candidate returned an invalid profile"))?;
    let installed = layout.base.join("spotiya").exists();
    let running = !installed
        || live_pid(&layout.base, Path::new("/run/spotify-connect.pid")).is_some()
        || live_pid(&layout.base, Path::new("/run/spotiya-supervisor.pid")).is_some();
    let mut mount = RootMount::writable()?;
    let result = transaction(&layout, root, binary, running, profile, &mut SystemService);
    mount.restore()?;
    let backup = result?;
    println!(
        "Installed spotiya {version}; {}. Backup: {}",
        if running {
            "receiver ready"
        } else {
            "receiver remains stopped"
        },
        backup.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<&'static str>,
        fail: bool,
    }
    impl Service for Fake {
        fn stop(&mut self) -> io::Result<()> {
            self.calls.push("stop");
            Ok(())
        }
        fn start(&mut self) -> io::Result<()> {
            self.calls.push("start");
            Ok(())
        }
        fn ready(&mut self, _: &str) -> io::Result<()> {
            self.calls.push("ready");
            if self.fail {
                self.fail = false;
                Err(error("fixture startup failed"))
            } else {
                Ok(())
            }
        }
    }
    fn fixture() -> (Temporary, Layout, PathBuf) {
        let temp = Temporary::new(&std::env::temp_dir(), "spotiya-install-test").unwrap();
        let layout = Layout {
            base: temp.0.join("installed"),
            init: temp.0.join("init"),
            cli: temp.0.join("cli"),
        };
        fs::create_dir(&layout.base).unwrap();
        let root = temp.0.join("package");
        for &f in FILES {
            let path = source(&root, &root.join("spotiya"), f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"new").unwrap();
            let old = layout.destination(f);
            fs::create_dir_all(old.parent().unwrap()).unwrap();
            fs::write(old, b"old").unwrap();
        }
        symlink(layout.base.join("cli.sh"), &layout.cli).unwrap();
        fs::create_dir(layout.base.join("receiver-state")).unwrap();
        fs::write(
            layout.base.join("receiver-state/credentials.json"),
            b"private fixture",
        )
        .unwrap();
        (temp, layout, root)
    }
    #[test]
    fn failed_start_restores_binary_scripts_and_profiles_and_recovers() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: true,
        };
        assert!(
            transaction(
                &layout,
                &root,
                &root.join("spotiya"),
                true,
                "mini2",
                &mut svc
            )
            .unwrap_err()
            .to_string()
            .contains("Rollback succeeded")
        );
        for &f in FILES {
            assert_eq!(fs::read(layout.destination(f)).unwrap(), b"old");
        }
        assert_eq!(
            svc.calls,
            ["stop", "start", "ready", "stop", "start", "ready"]
        );
        assert_eq!(
            fs::read(layout.base.join("receiver-state/credentials.json")).unwrap(),
            b"private fixture"
        );
    }
    #[test]
    fn first_install_creates_profile_directory_and_does_not_stop_a_missing_service() {
        let (_temp, layout, root) = fixture();
        for &file in FILES {
            fs::remove_file(layout.destination(file)).unwrap();
        }
        fs::remove_file(&layout.cli).unwrap();
        fs::remove_dir(layout.base.join("profiles")).unwrap();
        let mut service = Fake {
            calls: vec![],
            fail: false,
        };
        transaction(
            &layout,
            &root,
            &root.join("spotiya"),
            true,
            "mini1",
            &mut service,
        )
        .unwrap();
        assert_eq!(service.calls, ["start", "ready"]);
        assert_eq!(
            fs::read(layout.base.join("profiles/mini1.json")).unwrap(),
            b"new"
        );
        assert!(layout.init.is_file());
        assert_eq!(
            fs::read_link(&layout.cli).unwrap(),
            layout.base.join("cli.sh")
        );
    }
    #[test]
    fn stopped_receiver_stays_stopped_and_only_managed_backups_are_pruned() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: false,
        };
        fs::create_dir_all(layout.base.join("backups/manual")).unwrap();
        for _ in 0..4 {
            transaction(
                &layout,
                &root,
                &root.join("spotiya"),
                false,
                "mini2",
                &mut svc,
            )
            .unwrap();
        }
        assert!(svc.calls.is_empty());
        assert_eq!(fs::read(layout.base.join("spotiya")).unwrap(), b"new");
        assert_eq!(
            fs::read_dir(layout.base.join("backups")).unwrap().count(),
            KEEP + 1
        );
        assert!(layout.base.join("backups/manual").exists());
    }
}
