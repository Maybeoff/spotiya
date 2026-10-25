use crate::{
    control::Status,
    installation::{self, Temporary, error},
    update,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_ARCHIVE: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;
pub const AIRPLAY_BINS: &[&str] = &["shairport-sync", "nqptp", "avahi-daemon", "dbus-daemon"];

pub struct Addon {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub asset: &'static str,
    sums: &'static str,
    root: &'static str,
    directory: &'static str,
    bins: &'static [&'static str],
    pub command: &'static str,
    pub state: fn(&Status) -> State<'_>,
}
pub struct State<'a> {
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    pub error: &'a str,
}
impl State<'_> {
    pub fn label(&self) -> &'static str {
        if !self.installed {
            "Not installed"
        } else if !self.enabled {
            "Off"
        } else if !self.error.is_empty() {
            "Error"
        } else if self.running {
            "Running"
        } else {
            "Starting"
        }
    }
}
pub static CATALOG: &[Addon] = &[Addon {
    id: "airplay",
    name: "AirPlay 2",
    description: "Receive audio from iPhone, iPad and Mac. Disabled services use no RAM.",
    asset: "spotiya-airplay2-armv7.tar.gz",
    sums: "AIRPLAY_SHA256SUMS",
    root: "spotiya-airplay2",
    directory: "airplay",
    bins: AIRPLAY_BINS,
    command: "AIRPLAY",
    state: |s| State {
        installed: s.airplay_available,
        enabled: s.airplay_enabled,
        running: s.airplay_running,
        error: &s.airplay_error,
    },
}];
pub fn find(id: &str) -> io::Result<&'static Addon> {
    CATALOG
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| error("Unknown add-on"))
}
pub fn airplay_available(base: &Path) -> bool {
    AIRPLAY_BINS.iter().all(|name| {
        fs::metadata(base.join("airplay/bin").join(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    target: String,
    spotiya_version: String,
    files: BTreeMap<String, String>,
}
fn digest(path: &Path) -> io::Result<[u8; 32]> {
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 32768];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().into())
}
fn allowed(addon: &Addon, path: &Path) -> bool {
    let Some(p) = path.to_str() else {
        return false;
    };
    if p == "install.sh" {
        return true;
    }
    let Some(p) = p.strip_prefix(&format!("{}/", addon.directory)) else {
        return false;
    };
    matches!(
        p,
        "manifest.json"
            | "SHA256SUMS"
            | "sources.json"
            | "revisions.json"
            | "avahi-no-reverse.patch"
    ) || addon.bins.iter().any(|b| p == format!("bin/{b}"))
        || (p.starts_with("licenses/") && path.components().count() == 4)
}
fn extract(addon: &Addon, archive: &Path, stage: &Path, space: u64) -> io::Result<PathBuf> {
    let input = fs::File::open(archive)?;
    let mut tar =
        tar::Archive::new(flate2::read::GzDecoder::new(input).take(MAX_EXPANDED + 1024 * 1024));
    let mut total = 0u64;
    for (i, entry) in tar.entries()?.enumerate() {
        if i >= 256 {
            return Err(error("Too many add-on archive entries"));
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
            || !path.starts_with(addon.root)
        {
            return Err(error("Unsafe add-on archive path"));
        }
        let relative = path
            .strip_prefix(addon.root)
            .map_err(|_| error("Invalid add-on root"))?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() || !allowed(addon, relative) {
            return Err(error("Unexpected file or link in add-on archive"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| error("Add-on size overflow"))?;
        if total > MAX_EXPANDED {
            return Err(error("Expanded add-on exceeds 32 MiB"));
        }
        if total > space {
            return Err(error("Not enough disk space for add-on"));
        }
        let target = stage.join(relative);
        fs::create_dir_all(target.parent().unwrap())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
    }
    Ok(stage.join(addon.directory))
}
fn validate_manifest(addon: &Addon, payload: &Path) -> io::Result<()> {
    let text = update::limited_file(&payload.join("manifest.json"), 16384)?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|_| error("Invalid add-on manifest"))?;
    if manifest.format != 1
        || manifest.target != "armv7-unknown-linux-gnueabihf"
        || update::parse_version(&manifest.spotiya_version)?
            != update::parse_version(env!("CARGO_PKG_VERSION"))?
        || manifest.files.len() != addon.bins.len()
    {
        return Err(error(
            "Add-on does not match this Spotiya version or architecture",
        ));
    }
    for name in addon.bins {
        let path = format!("bin/{name}");
        let expected = manifest
            .files
            .get(&path)
            .ok_or_else(|| error("Incomplete add-on manifest"))?;
        let sum = update::checksum(&format!("{expected}  {path}"), &path)?;
        let file = payload.join(&path);
        if digest(&file)? != sum {
            return Err(error("Add-on binary SHA256 mismatch"));
        }
        fs::set_permissions(file, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
fn validate_binaries(addon: &Addon, payload: &Path) -> io::Result<()> {
    for name in addon.bins {
        let file = payload.join("bin").join(name);
        let mut header = [0; 20];
        fs::File::open(&file)?.read_exact(&mut header)?;
        if &header[..6] != b"\x7fELF\x01\x01" || header[18..20] != [40, 0] {
            return Err(error("Add-on requires ARMv7 Linux executables"));
        }
        let argument = if matches!(*name, "shairport-sync" | "nqptp") {
            "-V"
        } else {
            "--version"
        };
        let result = installation::command(&file, &[argument.as_ref()], Duration::from_secs(5))?;
        if !result.status.success() {
            return Err(error(format!("{name} cannot run on this station")));
        }
        if *name == "shairport-sync"
            && !String::from_utf8_lossy(&result.stdout).contains("AirPlay2")
        {
            return Err(error("The AirPlay package lacks AirPlay 2 support"));
        }
    }
    Ok(())
}
fn install_verified(
    addon: &Addon,
    base: &Path,
    archive: &Path,
    sums: &str,
    validate: impl FnOnce(&Addon, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if fs::metadata(archive)?.len() > MAX_ARCHIVE {
        return Err(error("Add-on archive exceeds 16 MiB"));
    }
    if digest(archive)? != update::checksum(sums, addon.asset)? {
        return Err(error("Add-on archive SHA256 mismatch"));
    }
    let _lock = crate::file_lock(&base.join("receiver-state"), "install.lock")
        .map_err(|_| error("Another installation is running"))?;
    let target = base.join(addon.directory);
    if target.symlink_metadata().is_ok() {
        return Err(error("Add-on already exists; enable it from Add-ons"));
    }
    let space = installation::free_bytes(base)?.saturating_sub(2 * 1024 * 1024);
    if space == 0 {
        return Err(error("Not enough disk space for add-on"));
    }
    let stage = Temporary::new(base, "addon-install")?;
    let payload = extract(addon, archive, &stage.0, space)?;
    validate_manifest(addon, &payload)?;
    validate(addon, &payload)?;
    fs::rename(payload, target)?;
    fs::File::open(base)?.sync_all()
}
pub fn local(id: &str, base: &Path, archive: &Path, sums: &Path) -> io::Result<()> {
    let addon = find(id)?;
    let stage = Temporary::new(Path::new("/tmp"), "addon-download")?;
    let copy = stage.0.join(addon.asset);
    let mut input = fs::File::open(archive)?.take(MAX_ARCHIVE + 1);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&copy)?;
    if io::copy(&mut input, &mut output)? > MAX_ARCHIVE {
        return Err(error("Add-on archive exceeds 16 MiB"));
    }
    drop(output);
    install_verified(
        addon,
        base,
        &copy,
        &update::limited_file(sums, 65536)?,
        validate_binaries,
    )
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
}
pub async fn remote(id: &str, base: &Path, mut progress: impl FnMut(String)) -> io::Result<()> {
    if !cfg!(target_arch = "arm") {
        return Err(error("Install add-ons on the ARM station"));
    }
    let addon = find(id)?;
    if !base.join("spotiya").is_file() {
        return Err(error("Install Spotiya first"));
    }
    let version = env!("CARGO_PKG_VERSION");
    update::parse_version(version)?;
    let tag = format!("v{version}");
    progress(format!("Checking GitHub release {tag}..."));
    let client = update::client()?;
    let stage = Temporary::new(Path::new("/tmp"), "addon-download")?;
    if installation::free_bytes(&stage.0)? < MAX_ARCHIVE + 2 * 1024 * 1024 {
        return Err(error("Not enough temporary space"));
    }
    let metadata = stage.0.join("release.json");
    let url = format!("https://api.github.com/repos/klockky/spotiya/releases/tags/{tag}");
    update::download_progress(&client, &url, &metadata, 1024 * 1024, |_, _| {})
        .await
        .map_err(|e| {
            if e.to_string().contains("HTTP 404") {
                error(format!("No published add-ons for {tag} yet"))
            } else {
                e
            }
        })?;
    let release: Release = serde_json::from_str(&update::limited_file(&metadata, 1024 * 1024)?)
        .map_err(|_| error("Invalid GitHub release metadata"))?;
    if release.draft || release.prerelease || release.tag_name != tag {
        return Err(error("Release does not match this Spotiya version"));
    }
    let archive = release
        .assets
        .iter()
        .find(|a| a.name == addon.asset && a.size > 0 && a.size <= MAX_ARCHIVE)
        .ok_or_else(|| error("This release has no compatible add-on package"))?;
    if !release
        .assets
        .iter()
        .any(|a| a.name == addon.sums && a.size > 0 && a.size <= 65536)
    {
        return Err(error("Release is missing add-on checksums"));
    }
    let download = format!("https://github.com/klockky/spotiya/releases/download/{tag}");
    let sums = stage.0.join(addon.sums);
    update::download_progress(
        &client,
        &format!("{download}/{}", addon.sums),
        &sums,
        65536,
        |_, _| {},
    )
    .await?;
    let path = stage.0.join(addon.asset);
    let mut last = 101;
    update::download_progress(
        &client,
        &format!("{download}/{}", addon.asset),
        &path,
        MAX_ARCHIVE,
        |n, _| {
            let percent = (n.saturating_mul(100) / archive.size).min(100);
            if percent != last {
                progress(format!(
                    "Downloading {}: {percent}% ({n}/{})",
                    addon.name, archive.size
                ));
                last = percent;
            }
        },
    )
    .await?;
    if fs::metadata(&path)?.len() != archive.size {
        return Err(error("Downloaded add-on size does not match release"));
    }
    progress("Verifying and installing add-on...".into());
    install_verified(
        addon,
        base,
        &path,
        &update::limited_file(&sums, 65536)?,
        validate_binaries,
    )
}

pub enum Message {
    Progress(String),
    Finished(io::Result<()>),
}
pub struct InstallJob {
    pub id: &'static str,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl InstallJob {
    pub fn start(
        addon: &'static Addon,
        messages: std::sync::mpsc::Sender<Message>,
    ) -> io::Result<Self> {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new().name("addon-install".into()).stack_size(512*1024).spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let result = runtime.block_on(async {
                    tokio::select! {
                        result = remote(addon.id, Path::new(installation::BASE), |text| { let _ = messages.send(Message::Progress(text)); }) => result,
                        _ = cancelled => Err(error("Add-on installation cancelled")),
                    }
                });
                runtime.shutdown_timeout(Duration::from_secs(1));
                result
            })();
            let _ = messages.send(Message::Finished(result));
        })?;
        Ok(Self {
            id: addon.id,
            cancel: Some(cancel),
            worker: Some(worker),
        })
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}
impl Drop for InstallJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Temporary {
        let dir = Temporary::new(&std::env::temp_dir(), "addon-test").unwrap();
        fs::create_dir(dir.0.join("receiver-state")).unwrap();
        fs::write(
            dir.0.join("receiver-state/credentials.json"),
            b"private credentials",
        )
        .unwrap();
        fs::write(dir.0.join("spotiya"), b"running receiver").unwrap();
        dir
    }
    fn archive(
        root: &Path,
        bad_manifest: bool,
        extra: Option<(&str, tar::EntryType)>,
    ) -> (PathBuf, String) {
        let addon = &CATALOG[0];
        let path = root.join(addon.asset);
        let mut files = BTreeMap::new();
        let mut hashes = BTreeMap::new();
        for name in addon.bins {
            let data = format!("binary {name}").into_bytes();
            hashes.insert(
                format!("bin/{name}"),
                format!("{:x}", Sha256::digest(&data)),
            );
            files.insert(format!("{}/airplay/bin/{name}", addon.root), data);
        }
        let manifest = serde_json::json!({"format":1,"target":"armv7-unknown-linux-gnueabihf",
            "spotiya_version":if bad_manifest {"0.0.0"} else {env!("CARGO_PKG_VERSION")},"files":hashes});
        files.insert(
            format!("{}/airplay/manifest.json", addon.root),
            serde_json::to_vec(&manifest).unwrap(),
        );
        let zip = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(zip);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, name, &data[..]).unwrap();
        }
        if let Some((name, kind)) = extra {
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b""[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let sum = format!(
            "{:x}  {}\n",
            Sha256::digest(fs::read(&path).unwrap()),
            addon.asset
        );
        (path, sum)
    }
    fn unchanged(base: &Path) {
        assert_eq!(fs::read(base.join("spotiya")).unwrap(), b"running receiver");
        assert_eq!(
            fs::read(base.join("receiver-state/credentials.json")).unwrap(),
            b"private credentials"
        );
        assert!(!fs::read_dir(base).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".addon-install-")
        }));
    }
    #[test]
    fn installs_only_addon_payload_and_does_not_restart_or_replace_spotify() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap();
        assert!(airplay_available(&dir.0));
        assert!(!dir.0.join("receiver-state/airplay-enabled").exists());
        unchanged(&dir.0);
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        assert!(airplay_available(&dir.0));
    }
    #[test]
    fn extraction_uses_actual_payload_space_and_rejects_a_smaller_budget() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, _) = archive(&dir.0, false, None);
        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(&path).unwrap()));
        let expanded = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().size())
            .sum::<u64>();
        assert!(expanded < 1024 * 1024);
        let enough = Temporary::new(&dir.0, "addon-space").unwrap();
        let payload = extract(addon, &path, &enough.0, expanded).unwrap();
        validate_manifest(addon, &payload).unwrap();
        let short = Temporary::new(&dir.0, "addon-space").unwrap();
        assert_eq!(
            extract(addon, &path, &short.0, expanded - 1)
                .unwrap_err()
                .to_string(),
            "Not enough disk space for add-on"
        );
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }

    #[test]
    fn corrupt_wrong_version_or_unrunnable_packages_leave_no_installed_payload() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, true, None);
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        let (path, sums) = archive(&dir.0, false, None);
        let unpacked = Temporary::new(&dir.0, "addon-inspect").unwrap();
        let payload = extract(addon, &path, &unpacked.0, MAX_EXPANDED).unwrap();
        fs::write(payload.join("bin/shairport-sync"), b"changed binary").unwrap();
        assert!(validate_manifest(addon, &payload).is_err());
        assert!(
            install_verified(addon, &dir.0, &path, &sums, |_, _| Err(error(
                "incompatible libc"
            )))
            .is_err()
        );
        fs::write(&path, b"corrupt").unwrap();
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }
    #[test]
    fn archives_reject_traversal_links_unexpected_files_and_duplicate_entries() {
        let addon = &CATALOG[0];
        for (name, kind) in [
            ("spotiya-airplay2/../../outside", tar::EntryType::Regular),
            (
                "spotiya-airplay2/airplay/bin/shairport-sync",
                tar::EntryType::Symlink,
            ),
            (
                "spotiya-airplay2/airplay/bin/shairport-sync",
                tar::EntryType::Regular,
            ),
            ("spotiya-airplay2/airplay/secret", tar::EntryType::Regular),
            ("outside/file", tar::EntryType::Regular),
        ] {
            let dir = fixture();
            let (path, sums) = archive(&dir.0, false, Some((name, kind)));
            assert!(
                install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err(),
                "{name}"
            );
            assert!(!dir.0.join("airplay").exists());
            unchanged(&dir.0);
        }
    }
    #[test]
    fn installation_respects_existing_lock_and_rejects_foreign_elf() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        let lock = crate::file_lock(&dir.0.join("receiver-state"), "install.lock").unwrap();
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        drop(lock);
        assert!(install_verified(addon, &dir.0, &path, &sums, validate_binaries).is_err());
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }
}
