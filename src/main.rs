use alsa::mixer::SelemId;
use futures_util::StreamExt;
use librespot::{
    connect::{ConnectConfig, LoadRequest, LoadRequestOptions, Spirc},
    core::{
        Error,
        cache::Cache,
        config::{DeviceType, SessionConfig},
        session::Session,
    },
    playback::{
        audio_backend,
        config::{AudioFormat, NormalisationMethod, PlayerConfig},
        mixer::{Mixer, MixerConfig},
        player::{Player, PlayerEvent},
    },
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
mod audio_guard;
mod auth;
mod control;
mod device;
mod ducking;
mod logging;
mod playback;
mod profiles;
mod recovery;
mod spectrum;
mod supervisor;
mod tui;
mod voice;
mod volume_guard;

struct Command {
    action: i32,
    reply: std::sync::mpsc::SyncSender<bool>,
}
struct Shared {
    running: AtomicBool,
    active: AtomicBool,
    paused: AtomicBool,
    volume: AtomicU16,
    commands: mpsc::UnboundedSender<Command>,
    spectrum: Arc<spectrum::Spectrum>,
    ducking: Arc<ducking::Ducking>,
}
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

#[unsafe(no_mangle)]
pub extern "C" fn mini2_receiver_running() -> i32 {
    SHARED
        .get()
        .is_some_and(|s| s.running.load(Ordering::Acquire)) as i32
}
/// # Safety
/// Non-null pointers must reference writable, aligned `i32` objects for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mini2_receiver_status(paused: *mut i32, volume: *mut i32) -> i32 {
    let Some(s) = SHARED.get() else {
        return 0;
    };
    if paused.is_null() || volume.is_null() {
        return 0;
    }
    unsafe {
        *paused = s.paused.load(Ordering::Relaxed) as i32;
        *volume = percent(s.volume.load(Ordering::Relaxed)) as i32;
    }
    s.active.load(Ordering::Acquire) as i32
}
/// # Safety
/// A non-null `levels` must reference four writable bytes for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mini2_receiver_spectrum(levels: *mut u8) -> i32 {
    if levels.is_null() {
        return 0;
    }
    let Some(shared) = SHARED.get() else {
        return 0;
    };
    if !shared.active.load(Ordering::Acquire) || shared.paused.load(Ordering::Relaxed) {
        return 0;
    }
    let Some(frame) = shared.spectrum.frame() else {
        return 0;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(frame.as_ptr(), levels, frame.len());
    }
    1
}
#[unsafe(no_mangle)]
pub extern "C" fn mini2_receiver_command(action: i32) -> i32 {
    let Some(s) = SHARED.get() else {
        return 0;
    };
    if !(0..=4).contains(&action) || !s.active.load(Ordering::Acquire) {
        return 0;
    }
    let (reply, response) = std::sync::mpsc::sync_channel(1);
    if s.commands.send(Command { action, reply }).is_err() {
        return 0;
    }
    response
        .recv_timeout(Duration::from_millis(400))
        .unwrap_or(false) as i32
}
fn percent(volume: u16) -> u16 {
    ((u32::from(volume) * 100 + 32767) / 65535) as u16
}
fn from_percent(value: u16) -> u16 {
    ((u32::from(value.min(100)) * 65535 + 50) / 100) as u16
}

struct DeviceMixer {
    shared: Arc<Shared>,
    handle: Mutex<Option<alsa::mixer::Mixer>>,
    audio: profiles::Audio,
}
impl DeviceMixer {
    fn with_control<T>(
        &self,
        action: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, alsa::Error>,
    ) -> Result<T, alsa::Error> {
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| alsa::Error::new("ALSA mixer lock", libc::EIO))?;
        if handle.is_none() {
            *handle = Some(alsa::mixer::Mixer::new(&self.audio.mixer_device, true)?);
        }
        let result = self.access(handle.as_ref().unwrap(), action);
        if result.is_err() {
            *handle = None;
        }
        result
    }
    fn access<T>(
        &self,
        mixer: &alsa::mixer::Mixer,
        action: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, alsa::Error>,
    ) -> Result<T, alsa::Error> {
        mixer.handle_events()?;
        let control = mixer
            .find_selem(&SelemId::new(
                &self.audio.mixer_control,
                self.audio.mixer_index,
            ))
            .ok_or_else(|| alsa::Error::new("ALSA mixer control", libc::ENOENT))?;
        action(&control)
    }
    fn read(&self) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            let value = control.get_playback_volume(self.audio.mixer_channel.alsa())?;
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            Ok(((value - min).clamp(0, max - min) * 65535 / (max - min)) as u16)
        })
    }
    fn write(&self, volume: u16) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            let value = min + (i64::from(volume) * (max - min) + 32767) / 65535;
            control.set_playback_volume_all(value)?;
            Ok(((value - min) * 65535 / (max - min)) as u16)
        })
    }
}
impl Mixer for DeviceMixer {
    fn open(config: MixerConfig) -> Result<Self, Error> {
        Ok(Self {
            shared: SHARED.get().expect("receiver initialized").clone(),
            handle: Mutex::new(None),
            audio: profiles::Audio {
                mixer_device: config.device,
                mixer_control: config.control,
                mixer_index: config.index,
                ..Default::default()
            },
        })
    }
    fn volume(&self) -> u16 {
        if self.shared.ducking.volume.holding() {
            return self.shared.volume.load(Ordering::Acquire);
        }
        self.read()
            .unwrap_or_else(|_| self.shared.volume.load(Ordering::Relaxed))
    }
    fn set_volume(&self, value: u16) {
        if value != self.shared.volume.load(Ordering::Acquire) {
            self.shared.ducking.volume.spotify_command();
        }
        if self.shared.ducking.volume.holding() {
            self.shared.volume.store(value, Ordering::Release);
            return;
        }
        if let Ok(actual) = self.write(value) {
            self.shared.volume.store(actual, Ordering::Release);
        } else {
            eprintln!("receiver: mixer volume write failed");
        }
    }
}

struct Options {
    state_dir: PathBuf,
    name: String,
    buttons: bool,
    voice: bool,
    supervise: bool,
    profile: String,
}
fn options() -> Result<Option<Options>, String> {
    let mut result = Options {
        state_dir: "/data/opt/spotify-connect/receiver-state".into(),
        name: device::system_name(),
        buttons: true,
        voice: true,
        supervise: false,
        profile: "auto".into(),
    };
    let mut args = std::env::args().skip(1);
    let mut profile_info = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                result.state_dir = args.next().ok_or("missing state directory")?.into()
            }
            "--name" => result.name = args.next().ok_or("missing device name")?,
            "--profile" => result.profile = args.next().ok_or("missing profile id or JSON path")?,
            "--profile-info" => profile_info = true,
            "--profiles" => {
                profiles::list();
                return Ok(None);
            }
            "--supervise" => result.supervise = true,
            "--no-buttons" => result.buttons = false,
            "--no-voice" => result.voice = false,
            "--tui" => {
                tui::run().map_err(|e| e.to_string())?;
                return Ok(None);
            }
            "--login" => {
                use std::io::BufRead;
                let reader = control::login().map_err(|e| e.to_string())?;
                for line in reader.lines() {
                    let line = line.map_err(|e| e.to_string())?;
                    println!("{}", control::login_line(&line));
                    if line.starts_with("DONE") {
                        return Ok(None);
                    }
                    if line.starts_with("ERROR") {
                        return Err("Login was not completed".into());
                    }
                }
                return Err("Pairing connection closed before login completed".into());
            }
            "--play" | "--resume" | "--pause" | "--next" | "--previous" | "--toggle"
            | "--volume" | "--voice" | "--spectrum" | "--release" | "--retry" => {
                let command = match arg.as_str() {
                    "--play" => format!("PLAY {}", args.next().ok_or("missing music query")?),
                    "--resume" => "RESUME".to_owned(),
                    "--pause" => "PAUSE".to_owned(),
                    "--next" => "NEXT".to_owned(),
                    "--previous" => "PREVIOUS".to_owned(),
                    "--toggle" => "TOGGLE".to_owned(),
                    "--release" => "RELEASE".to_owned(),
                    "--retry" => "RETRY".to_owned(),
                    "--volume" => format!("VOLUME {}", args.next().ok_or("missing volume 0..100")?),
                    _ => format!(
                        "{} {}",
                        arg.trim_start_matches("--").to_uppercase(),
                        args.next().ok_or("missing on/off")?.to_uppercase()
                    ),
                };
                if control::parse(&command).is_none() {
                    return Err("Invalid command or value".into());
                }
                print!("{}", control::request(&command).map_err(|e| e.to_string())?);
                return Ok(None);
            }
            "--version" => {
                println!("spotiya {} (librespot 0.8.0)", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--status" => {
                print!(
                    "{}",
                    fs::read_to_string("/run/spotiya.status").map_err(|e| e.to_string())?
                );
                return Ok(None);
            }
            "--help" | "-h" => {
                println!(
                    "spotiya [--state-dir PATH] [--name NAME] [--profile auto|mini2|generic|FILE] [--no-buttons] [--no-voice]\nspotiya --profiles | --profile-info\nspotiya --tui | --login | --status | --version\nspotiya --play QUERY | --resume | --pause | --toggle | --next | --previous\nspotiya --volume 0..100 | --voice on/off | --spectrum on/off | --release | --retry"
                );
                return Ok(None);
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    if profile_info {
        let resolved = resolved_profile(&result).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&resolved).map_err(|e| e.to_string())?
        );
        return Ok(None);
    }
    Ok(Some(result))
}
fn resolved_profile(options: &Options) -> io::Result<profiles::Resolved> {
    let mut resolved = profiles::load(&options.profile, &options.state_dir)?;
    if !options.buttons {
        resolved.capabilities.buttons = false;
        resolved.capabilities.ring = false;
        resolved.capabilities.display = false;
    }
    Ok(resolved)
}
fn file_lock(dir: &Path, name: &str) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(name))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
fn publish_status(
    s: &Shared,
    playback: &playback::Playback,
    failures: &recovery::Failures,
    info: &mut control::Status,
    last: &mut Vec<u8>,
    text: &mut Vec<u8>,
) {
    s.paused.store(playback.paused(), Ordering::Relaxed);
    s.active.store(playback.active(), Ordering::Release);
    info.loading = playback.loading();
    info.retry_required = failures.blocked();
    info.retry_after_secs = failures.retry_after();
    info.failed_tracks = failures.count();
    info.error = if failures.blocked() {
        failures.message()
    } else {
        playback.error().to_owned()
    };
    info.position_ms = playback.position_ms().min(info.duration_ms);
    info.active = s.active.load(Ordering::Acquire);
    s.ducking.volume.set_enabled(info.active);
    s.ducking.volume.activity(s.ducking.active());
    info.paused = s.paused.load(Ordering::Relaxed);
    info.volume = percent(s.volume.load(Ordering::Relaxed));
    info.spectrum = s.spectrum.enabled();
    info.ducking = playback.playing() && s.ducking.active();
    text.clear();
    serde_json::to_writer(&mut *text, info).unwrap();
    text.push(b'\n');
    if last == text {
        return;
    }
    if fs::write("/run/spotiya.status.new", &*text)
        .and_then(|_| fs::rename("/run/spotiya.status.new", "/run/spotiya.status"))
        .is_ok()
    {
        std::mem::swap(last, text);
    }
}
async fn stop_connection(
    spirc: &mut Option<Spirc>,
    task: &mut Option<JoinHandle<()>>,
    session: &Session,
    shared: &Shared,
    playback: &mut playback::Playback,
) {
    playback.reset();
    shared.paused.store(false, Ordering::Relaxed);
    shared.active.store(false, Ordering::Release);
    if let Some(handle) = spirc.take() {
        let _ = handle.shutdown();
    }
    if let Some(mut task) = task.take()
        && tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
    {
        task.abort();
    }
    session.shutdown();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        libc::umask(0o077);
        libc::mallopt(libc::M_ARENA_MAX, 1);
        // Player creates its own multi-thread runtime after this single-threaded entry point.
        if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
            std::env::set_var("TOKIO_WORKER_THREADS", "1");
        }
    }
    let Some(options) = options().map_err(io::Error::other)? else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .thread_stack_size(512 * 1024)
        .thread_keep_alive(Duration::from_secs(2))
        .build()?
        .block_on(run(options))
}
async fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    if options.supervise {
        let _ = resolved_profile(&options)?;
        supervisor::run(&options).await?;
        return Ok(());
    }
    let _lock = file_lock(&options.state_dir, "receiver.lock")?;
    logging::init()?;
    let resolved = resolved_profile(&options)?;
    let profile = resolved.profile;
    let caps = resolved.capabilities;
    let native = profile.voice.as_ref().map(|v| v.address);
    let voice_setting = options.state_dir.join("voice-enabled");
    let mut voice_enabled = options.voice
        && caps.voice
        && fs::read_to_string(&voice_setting)
            .map(|s| s.trim() != "0")
            .unwrap_or(true);
    let spectrum_setting = options.state_dir.join("spectrum-enabled");
    let spectrum_supported = caps.display;
    let spectrum_enabled =
        spectrum_supported && fs::read_to_string(&spectrum_setting).is_ok_and(|s| s.trim() == "1");
    let last_track_file = options.state_dir.join("last-track");
    let mut last_track = fs::read_to_string(&last_track_file)
        .ok()
        .and_then(|s| voice::playable_track(&s));
    let cache = Cache::new(Some(&options.state_dir), None, None, None)?;
    let mut config = SessionConfig::default();
    let id_file = options.state_dir.join("device-id");
    if let Ok(id) = fs::read_to_string(&id_file) {
        if !id.trim().is_empty() {
            config.device_id = id.trim().to_owned();
        }
    } else {
        fs::write(&id_file, &config.device_id)?;
    }
    config.tmp_dir = "/tmp/spotiya".into();
    fs::create_dir_all(&config.tmp_dir)?;
    let mut discovery = librespot::discovery::Discovery::builder(
        config.device_id.clone(),
        config.client_id.clone(),
    )
    .name(options.name.clone())
    .device_type(DeviceType::Speaker)
    .port(10337)
    .launch()?;
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        running: AtomicBool::new(true),
        active: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        volume: AtomicU16::new(0),
        commands,
        spectrum: Arc::new(spectrum::Spectrum::new(spectrum_enabled)),
        ducking: Arc::new(ducking::Ducking::new()),
    });
    let _ = SHARED.set(shared.clone());
    let mixer = Arc::new(DeviceMixer {
        shared: shared.clone(),
        handle: Mutex::new(None),
        audio: profile.audio.clone(),
    });
    let initial = mixer.read().map_err(|e| {
        io::Error::other(format!(
            "Cannot open ALSA mixer '{}' on '{}': {e}; check the device profile",
            profile.audio.mixer_control, profile.audio.mixer_device
        ))
    })?;
    shared.volume.store(initial, Ordering::Relaxed);
    let mut session = Session::new(config.clone(), Some(cache.clone()));
    let player_config = PlayerConfig {
        normalisation: true,
        normalisation_method: NormalisationMethod::Basic,
        ..Default::default()
    };
    let backend = audio_backend::find(Some("alsa".into())).expect("ALSA compiled");
    let visual = shared.spectrum.clone();
    let ducking = shared.ducking.clone();
    let audio_device = profile.audio.device.clone();
    let audio_format = profile
        .audio
        .format
        .parse::<AudioFormat>()
        .map_err(|_| io::Error::other("Invalid profile audio format"))?;
    let player = Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || {
            Box::new(spectrum::VisualSink::new(
                backend(Some(audio_device), audio_format),
                visual.clone(),
                ducking.clone(),
            ))
        },
    );
    let mut events = player.get_player_event_channel();
    let (voice_tx, mut voice_rx) = mpsc::channel(8);
    let voice_watch = native.map(|address| {
        tokio::spawn(voice::watch(
            voice_tx.clone(),
            shared.ducking.clone(),
            address,
        ))
    });
    let (login_tx, mut login_rx) = mpsc::channel::<auth::Login>(1);
    let control_config = config.clone();
    let voice_control = tokio::spawn(async move {
        if control::serve(voice_tx, login_tx, control_config, caps)
            .await
            .is_err()
        {
            eprintln!("voice: local control socket unavailable");
        }
    });
    let audio_guard = Arc::new(audio_guard::AudioGuard::new());
    let audio_worker = native.map(|address| {
        let guard = audio_guard.clone();
        tokio::spawn(async move {
            guard.run(address).await;
        })
    });
    let controls = if caps.controls() {
        let config = profile
            .controls
            .clone()
            .expect("controls capability requires a driver");
        Some(
            std::thread::Builder::new()
                .name("spotiya-controls".into())
                .stack_size(128 * 1024)
                .spawn(move || config.run(caps))?,
        )
    } else {
        None
    };
    let mut credentials = cache.credentials();
    let mut spirc: Option<Spirc> = None;
    let mut task: Option<JoinHandle<()>> = None;
    let mut retry_at = Instant::now();
    let mut backoff = 5;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut pending_login = None::<tokio::sync::oneshot::Sender<Result<(), &'static str>>>;
    let mut last_status = Vec::new();
    let mut status_buffer = Vec::new();
    let mut info = control::Status {
        name: options.name.clone(),
        voice: voice_enabled,
        voice_supported: caps.voice,
        profile: profile.id.clone(),
        spectrum_supported,
        ..Default::default()
    };
    let mut playback = playback::Playback::default();
    let mut failures = recovery::Failures::default();
    let mut retry_track = None::<String>;
    let mut reclaim_at = Instant::now() + Duration::from_secs(30);
    eprintln!(
        "receiver: ready, profile={}, buttons={}, ring={}, display={}, voice={}",
        profile.id, caps.buttons, caps.ring, caps.display, caps.voice
    );
    loop {
        tokio::select! {
            _=terminate.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            fresh=discovery.next()=>{
                let Some(fresh)=fresh else { eprintln!("receiver: discovery stopped"); break; };
                audio_guard.set_playing(false);
                stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                credentials=Some(fresh); retry_at=Instant::now(); backoff=5;
            },
            Some(login)=login_rx.recv()=>{
                audio_guard.set_playing(false);
                stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                failures.reset();retry_track=None;
                credentials=Some(login.credentials);pending_login=Some(login.completed);
                retry_at=Instant::now();backoff=5;
            },
            _=tokio::time::sleep_until(retry_at), if spirc.is_none() && credentials.is_some() && !failures.blocked()=>{
                session=Session::new(config.clone(),Some(cache.clone()));
                player.set_session(session.clone());
                let connect=ConnectConfig { name:options.name.clone(), initial_volume:mixer.volume(),
                    volume_steps:100, device_type:DeviceType::Speaker, ..Default::default() };
                let attempt=Spirc::new(connect,session.clone(),credentials.clone().unwrap(),player.clone(),mixer.clone());
                let result=tokio::select! {
                    _=terminate.recv()=>break,
                    _=tokio::signal::ctrl_c()=>break,
                    result=tokio::time::timeout(Duration::from_secs(30),attempt)=>result,
                };
                match result {
                    Ok(Ok((handle,future)))=>{
                        spirc=Some(handle);task=Some(tokio::spawn(future));backoff=5;eprintln!("receiver: Spotify connected");
                        if let Some(reply)=pending_login.take(){let _=reply.send(Ok(()));}
                        if let Some(uri)=retry_track.take() && let Some(handle)=spirc.as_ref() {
                            let _=handle.activate();
                            let request=LoadRequest::from_tracks(vec![uri],LoadRequestOptions{start_playing:true,..Default::default()});
                            let _=handle.load(request);
                        }
                    },
                    Ok(Err(e))=>{
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err("Receiver could not connect the new account"));}
                        eprintln!("receiver: connect failed ({:?}); discovery remains available",e.kind);
                        if e.kind==librespot::core::error::ErrorKind::Unauthenticated {credentials=None;}
                        session.shutdown(); retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);
                    },
                    Err(_)=>{
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err("Receiver connect timed out"));}
                        session.shutdown();retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);eprintln!("receiver: connect timeout");
                    },
                }
            },
            _=async {if let Some(task)=task.as_mut(){let _=task.await;}}, if task.is_some()=>{
                spirc=None;task=None;session.shutdown();playback.reset();
                retry_at=Instant::now()+Duration::from_secs(5);eprintln!("receiver: reconnect scheduled");
            },
            Some(event)=events.recv()=>{
                if failures.blocked() || !playback.update(&event) {continue;}
                if failures.observe(&event) {
                    log::warn!("receiver: {}",failures.message());
                    audio_guard.set_playing(false);player.stop();
                    stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                    while events.try_recv().is_ok() {}
                }
                match event {
                    PlayerEvent::PlayRequestIdChanged{..}=>{
                        info.title.clear();info.artist.clear();info.album.clear();info.duration_ms=0;
                    },
                    PlayerEvent::Playing{track_id,..}=>{
                        if let Some(uri)=track_id.to_uri().ok().and_then(|s| voice::playable_track(&s))
                            && last_track.as_ref()!=Some(&uri) {
                                let temp=last_track_file.with_extension("new");
                                if fs::write(&temp,&uri).and_then(|_| fs::rename(&temp,&last_track_file)).is_ok() {
                                    last_track=Some(uri);
                                } else {eprintln!("receiver: could not save last track");}
                            }
                    },
                    PlayerEvent::TrackChanged{audio_item}=>{
                        info.title=audio_item.name;info.duration_ms=audio_item.duration_ms;
                        match audio_item.unique_fields {
                            librespot::metadata::audio::UniqueFields::Track{artists,album,..}=>{info.artist=artists.iter().map(|a|a.name.as_str()).collect::<Vec<_>>().join(", ");info.album=album;},
                            librespot::metadata::audio::UniqueFields::Episode{show_name,..}=>{info.artist=show_name;info.album.clear();},
                            librespot::metadata::audio::UniqueFields::Local{artists,album,..}=>{info.artist=artists.unwrap_or_default();info.album=album.unwrap_or_default();},
                        }
                    },
                    PlayerEvent::VolumeChanged{..}=>{shared.volume.store(mixer.volume(),Ordering::Release);},
                    _=>{},
                }
            },
            Some(command)=command_rx.recv()=>{
                let ok=if let Some(handle)=spirc.as_ref().filter(|_| shared.active.load(Ordering::Acquire)) {
                    match command.action {
                        0=>handle.play_pause().is_ok(),
                        1=>handle.next().is_ok(),
                        2=>handle.prev().is_ok(),
                        3|4=>{
                            let old=percent(mixer.volume()) as i32;
                            let target=(old+if command.action==3 {5}else{-5}).clamp(0,100) as u16;
                            mixer.set_volume(from_percent(target));
                            handle.set_volume(mixer.volume()).is_ok()
                        },
                        _=>false,
                    }
                }else{false};
                let _=command.reply.send(ok);
            },
            Some(event)=voice_rx.recv()=>{
                if event.received.elapsed()>Duration::from_secs(5) {continue;}
                if event.from_voice && !voice_enabled {continue;}
                let intent=match event.intent {
                    voice::Intent::Retry=>{
                        if let Some(uri)=failures.retry() {
                            retry_track=Some(uri);retry_at=Instant::now();backoff=5;
                            eprintln!("receiver: explicit retry scheduled");
                        } else {eprintln!("receiver: retry unavailable or cooling down");}
                        publish_status(&shared,&playback,&failures,&mut info,&mut last_status,&mut status_buffer);
                        continue;
                    },
                    voice::Intent::Voice(enabled)=>{
                        if !caps.voice {continue;}
                        if fs::write(&voice_setting,if enabled{"1\n"}else{"0\n"}).is_ok(){voice_enabled=enabled;info.voice=enabled;}
                        else{eprintln!("voice: could not save voice setting");}
                        continue;
                    },
                    voice::Intent::Spectrum(enabled)=>{
                        if spectrum_supported {
                            let temp=spectrum_setting.with_extension("new");
                            if fs::write(&temp,if enabled{"1\n"}else{"0\n"})
                                .and_then(|_| fs::rename(&temp,&spectrum_setting)).is_ok() {
                                shared.spectrum.set_enabled(enabled);
                            } else { eprintln!("display: could not save spectrum setting"); }
                        }
                        continue;
                    },
                    voice::Intent::Volume(volume)=>{
                        mixer.set_volume(from_percent(volume));
                        if let Some(handle)=spirc.as_ref(){let _=handle.set_volume(mixer.volume());}
                        continue;
                    },
                    voice::Intent::Release=>{
                        if let Some(handle)=spirc.as_ref(){let _=handle.disconnect(true);}
                        playback.reset();shared.paused.store(false,Ordering::Relaxed);
                        shared.active.store(false,Ordering::Release);audio_guard.set_playing(false);
                        continue;
                    },
                    voice::Intent::Toggle=>if shared.active.load(Ordering::Acquire)&&!shared.paused.load(Ordering::Relaxed){voice::Intent::Pause}else{voice::Intent::Resume},
                    intent=>intent,
                };
                if failures.blocked() {continue;}
                let Some(handle)=spirc.as_ref() else {eprintln!("voice: Spotify is not connected yet");continue;};
                match intent {
                    voice::Intent::Play(query)=>{
                        if voice::prepare_playback(native,event.from_voice).await.is_err() {eprintln!("voice: could not cancel native conversation");continue;}
                        let _=handle.activate();
                        let request=LoadRequest::from_context_uri(voice::context(&query),LoadRequestOptions{start_playing:true,..Default::default()});
                        if handle.load(request).is_ok() {eprintln!("voice: Spotify music query queued");}
                    },
                    voice::Intent::Resume if !shared.active.load(Ordering::Acquire) && last_track.is_some()=>{
                        if voice::prepare_playback(native,event.from_voice).await.is_err() {continue;}
                        let _=handle.activate();
                        let request=LoadRequest::from_tracks(vec![last_track.clone().unwrap()],LoadRequestOptions{start_playing:true,..Default::default()});
                        if handle.load(request).is_ok() {eprintln!("voice: last Spotify track queued");}
                    },
                    intent if shared.active.load(Ordering::Acquire)=>{
                        let _=voice::prepare_playback(native,event.from_voice).await;
                        match intent {
                            voice::Intent::Pause=>{let _=handle.pause();},
                            voice::Intent::Resume=>{let _=handle.play();},
                            voice::Intent::Next=>{let _=handle.next();},
                            voice::Intent::Previous=>{let _=handle.prev();},
                            _=>{},
                        }
                        eprintln!("voice: Spotify transport command queued");
                    },
                    _=>{},
                }
            },
            _=tick.tick()=>{
                if !playback.playing() && Instant::now() >= reclaim_at {
                    unsafe { libc::malloc_trim(0); }
                    reclaim_at=Instant::now()+Duration::from_secs(30);
                }
                let guard=&shared.ducking.volume;
                guard.set_enabled(playback.active());guard.activity(shared.ducking.active());
                if let Ok(value)=mixer.read() {
                    if guard.holding() {
                        let desired=shared.volume.load(Ordering::Acquire);
                        if let Some(target)=guard.restore_target(value,desired)
                            && let Ok(actual)=if target==value {Ok(value)} else {mixer.write(target)} {
                                shared.volume.store(actual,Ordering::Release);guard.restored();
                                if let Some(handle)=spirc.as_ref(){let _=handle.set_volume(actual);}
                                eprintln!("volume: restored after Alice conversation");
                        }
                    } else {
                        let previous=shared.volume.swap(value,Ordering::AcqRel);
                        if value!=previous && shared.active.load(Ordering::Acquire)
                            && let Some(handle)=spirc.as_ref() {let _=handle.set_volume(value);}
                    }
                }
                if player.is_invalid() {eprintln!("receiver: audio worker stopped");break;}
            },
        }
        let playing = playback.playing();
        audio_guard.set_playing(playing);
        info.connected = spirc.is_some();
        info.native_guard = playing && audio_guard.healthy();
        publish_status(
            &shared,
            &playback,
            &failures,
            &mut info,
            &mut last_status,
            &mut status_buffer,
        );
    }
    shared.active.store(false, Ordering::Release);
    shared.running.store(false, Ordering::Release);
    if let Some(worker) = voice_watch {
        worker.abort();
    }
    audio_guard.set_playing(false);
    if let Some(worker) = audio_worker {
        worker.abort();
    }
    voice_control.abort();
    let _ = fs::remove_file(voice::CONTROL);
    stop_connection(&mut spirc, &mut task, &session, &shared, &mut playback).await;
    if let Some(controls) = controls {
        let _ = controls.join();
    }
    player.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), discovery.shutdown()).await;
    info.connected = false;
    info.native_guard = false;
    publish_status(
        &shared,
        &playback,
        &failures,
        &mut info,
        &mut last_status,
        &mut status_buffer,
    );
    eprintln!("receiver: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_volume_percentages_round_trip_through_master() {
        for p in 0..=100 {
            let normalized = from_percent(p);
            let raw = (u32::from(normalized) * 255 + 32767) / 65535;
            assert_eq!(percent((raw * 65535 / 255) as u16), p);
        }
    }
    #[tokio::test]
    async fn controls_bridge_checks_ownership_and_acknowledges_in_process_commands() {
        assert_eq!(mini2_receiver_command(0), 0);
        assert_eq!(mini2_receiver_command(99), 0);
        assert_eq!(mini2_receiver_running(), 0);
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(true),
            volume: AtomicU16::new(from_percent(37)),
            commands,
            spectrum: Arc::new(spectrum::Spectrum::new(false)),
            ducking: Arc::new(ducking::Ducking::new()),
        });
        assert!(SHARED.set(shared.clone()).is_ok());
        assert_eq!(mini2_receiver_command(0), 0);
        shared.active.store(true, Ordering::Release);
        assert_eq!(mini2_receiver_command(99), 0);
        let worker = std::thread::spawn(|| mini2_receiver_command(3));
        let command = receiver.recv().await.unwrap();
        assert_eq!(command.action, 3);
        command.reply.send(true).unwrap();
        assert_eq!(worker.join().unwrap(), 1);
        let (mut paused, mut volume) = (0, 0);
        assert_eq!(
            unsafe { mini2_receiver_status(&mut paused, &mut volume) },
            1
        );
        assert_eq!((paused, volume), (1, 37));
        let mut levels = [0; 4];
        assert_eq!(unsafe { mini2_receiver_spectrum(std::ptr::null_mut()) }, 0);
        assert_eq!(unsafe { mini2_receiver_spectrum(levels.as_mut_ptr()) }, 0);
        shared.active.store(false, Ordering::Release);
        shared.running.store(false, Ordering::Release);
        assert_eq!(mini2_receiver_command(3), 0);
        assert_eq!(mini2_receiver_running(), 0);
    }
}
