use crate::control::{self, Status};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::fd::AsRawFd,
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Terminal {
    saved: libc::termios,
}
impl Terminal {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "TUI needs a terminal; use adb shell -t or spotiya tui",
            ));
        }
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        print!("\x1b[?1049h\x1b[?25l");
        io::stdout().flush()?;
        if let Ok(mut active) = ACTIVE.lock() {
            *active = Some(saved);
        }
        Ok(Self { saved })
    }
}
/// Terminal settings to put back if the TUI panics, since release builds abort without `Drop`.
static ACTIVE: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);
fn leave(saved: &libc::termios) {
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, saved);
    }
    print!("\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = io::stdout().flush();
}
pub fn restore_terminal_on_panic() {
    if let Ok(mut active) = ACTIVE.try_lock()
        && let Some(saved) = active.take()
    {
        leave(&saved);
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            *active = None;
        }
        leave(&self.saved);
    }
}
fn size() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size);
    }
    (
        if size.ws_col == 0 {
            80
        } else {
            usize::from(size.ws_col).max(20)
        },
        if size.ws_row == 0 {
            24
        } else {
            usize::from(size.ws_row).max(5)
        },
    )
}
fn safe(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
fn clock(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
fn lines(status: &Status, notice: &str, prompt: Option<&[u8]>, width: usize) -> Vec<String> {
    let state = if status.airplay_active {
        "AIRPLAY 2"
    } else if status.retry_required {
        "RETRY REQUIRED"
    } else if !status.connected {
        "CONNECTING"
    } else if !status.error.is_empty() {
        "UNAVAILABLE"
    } else if status.loading {
        "LOADING"
    } else if !status.active {
        "READY"
    } else if status.paused {
        "PAUSED"
    } else {
        "PLAYING"
    };
    let voice = if !status.voice_supported {
        "unavailable"
    } else if status.voice {
        "on"
    } else {
        "off"
    };
    let guard = if status.airplay_active {
        "AirPlay priority"
    } else if status.native_guard {
        "Spotify priority"
    } else {
        "idle"
    };
    let cells = width.saturating_sub(18).min(42);
    let filled = if status.duration_ms == 0 {
        0
    } else {
        ((u64::from(status.position_ms) * cells as u64) / u64::from(status.duration_ms))
            .min(cells as u64) as usize
    };
    let mut out = vec![
        format!(
            "SPOTIYA                         v{}",
            env!("CARGO_PKG_VERSION")
        ),
        String::new(),
        format!("{}   |   {}", safe(&status.name), state),
        format!("Profile: {}", safe(&status.profile)),
        format!(
            "Volume {:3}%   Voice {}   Audio: {}{}",
            status.volume,
            voice,
            guard,
            if status.ducking {
                "   Alice: ducked"
            } else {
                ""
            }
        ),
        format!(
            "Spectrum: {}",
            if !status.spectrum_supported {
                "unavailable"
            } else if status.spectrum {
                "on"
            } else {
                "off"
            }
        ),
        format!(
            "AirPlay 2: {}{}",
            if !status.airplay_available {
                "not installed"
            } else if !status.airplay_enabled {
                "off"
            } else if status.airplay_running {
                "on"
            } else {
                "starting"
            },
            if status.airplay_error.is_empty() {
                String::new()
            } else {
                format!(" ({})", safe(&status.airplay_error))
            }
        ),
        String::new(),
        safe(if status.airplay_active {
            "AirPlay audio"
        } else if status.title.is_empty() {
            "No track selected"
        } else {
            &status.title
        }),
        safe(&status.artist),
        safe(&status.album),
        format!(
            "{} [{}{}] {}",
            clock(status.position_ms),
            "=".repeat(filled),
            "-".repeat(cells - filled),
            clock(status.duration_ms)
        ),
        String::new(),
        "Space pause/play   n next   p previous   +/- volume".into(),
        "/ search   l login   v voice   e spectrum on/off   a add-ons".into(),
        "r retry   d device profile   x release to Alice   q quit".into(),
        String::new(),
    ];
    if !status.error.is_empty() {
        out.push(safe(&status.error));
        if status.retry_required {
            out.push(if status.retry_after_secs > 0 {
                format!("Retry available in {}s", status.retry_after_secs)
            } else {
                "Press r to retry".into()
            });
        }
    }
    out.extend(notice.lines().map(safe));
    if let Some(input) = prompt {
        out.push(format!(
            "Search: {}_",
            safe(&String::from_utf8_lossy(input))
        ));
    }
    out
}
fn draw(status: &Status, notice: &str, prompt: Option<&[u8]>) -> io::Result<()> {
    let (width, height) = size();
    draw_lines(lines(status, notice, prompt, width), width, height)
}
fn draw_lines(content: Vec<String>, width: usize, height: usize) -> io::Result<()> {
    let mut frame = String::from("\x1b[H");
    for (i, line) in content
        .into_iter()
        .take(height.saturating_sub(1))
        .enumerate()
    {
        if i == 0 {
            frame.push_str("\x1b[1;32m");
        }
        // Most metadata is Latin/Cyrillic; conservative clipping leaves room for
        // double-width characters and keeps control sequences out of the terminal.
        let mut used = 0;
        for c in line.chars() {
            let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
            if used + cells > width.saturating_sub(1) {
                break;
            }
            frame.push(c);
            used += cells;
        }
        frame.push_str("\x1b[0m\x1b[K\r\n");
    }
    frame.push_str("\x1b[J");
    print!("{frame}");
    io::stdout().flush()
}
struct ProfileMenu {
    entries: Vec<crate::profile_selection::Choice>,
    selected: usize,
    escape: u8,
    escape_at: Instant,
    current: String,
    notice: String,
}
enum ProfileAction {
    Apply,
    Cancel,
}
impl ProfileMenu {
    fn new(state: &Path, current: &str) -> io::Result<Self> {
        let entries = crate::profile_selection::choices(state)?;
        let selected = entries.iter().position(|c| c.key() == current).unwrap_or(0);
        Ok(Self {
            entries,
            selected,
            escape: 0,
            escape_at: Instant::now(),
            current: current.into(),
            notice: String::new(),
        })
    }
    fn key(&mut self, key: u8) -> Option<ProfileAction> {
        if key == 27 {
            self.escape = 1;
            self.escape_at = Instant::now();
            return None;
        }
        if self.escape == 1 && key == b'[' {
            self.escape = 2;
            return None;
        }
        let key = if self.escape == 2 {
            self.escape = 0;
            match key {
                b'A' => b'k',
                b'B' => b'j',
                _ => return None,
            }
        } else {
            key
        };
        match key {
            b'q' => Some(ProfileAction::Cancel),
            b'\r' | b'\n' => Some(ProfileAction::Apply),
            b'j' | b'n' => {
                self.selected = (self.selected + 1) % self.entries.len();
                None
            }
            b'k' | b'p' => {
                self.selected = (self.selected + self.entries.len() - 1) % self.entries.len();
                None
            }
            _ => None,
        }
    }
    fn lines(&self, height: usize) -> Vec<String> {
        let count = height.saturating_sub(9).max(1);
        let start = self.selected.saturating_sub(count - 1);
        let mut out = vec!["SPOTIYA — DEVICE PROFILE".into(), String::new()];
        for (index, choice) in self.entries.iter().enumerate().skip(start).take(count) {
            out.push(format!(
                "{} {}{}",
                if index == self.selected { ">" } else { " " },
                safe(&choice.name),
                if choice.key() == self.current {
                    " [current]"
                } else {
                    ""
                }
            ));
        }
        out.push(String::new());
        out.push(safe(&self.entries[self.selected].detail));
        out.push("Up/down or j/k: choose   Enter: apply   Esc/q: back".into());
        out.push("Applying restarts a running receiver and stops playback.".into());
        out.push(safe(&self.notice));
        out
    }
    fn draw(&self) -> io::Result<()> {
        let (width, height) = size();
        draw_lines(self.lines(height), width, height)
    }
}
#[derive(Default)]
struct AddonMenu {
    selected: usize,
    escape: u8,
    escape_at: Option<Instant>,
}
enum AddonAction {
    Apply,
    Cancel,
    StopInstall,
}
impl AddonMenu {
    fn key(&mut self, key: u8) -> Option<AddonAction> {
        if key == 27 {
            self.escape = 1;
            self.escape_at = Some(Instant::now());
            return None;
        }
        if self.escape == 1 && key == b'[' {
            self.escape = 2;
            return None;
        }
        let key = if self.escape == 2 {
            self.escape = 0;
            match key {
                b'A' => b'k',
                b'B' => b'j',
                _ => return None,
            }
        } else {
            key
        };
        match key {
            b'q' => Some(AddonAction::Cancel),
            b'c' => Some(AddonAction::StopInstall),
            b'\r' | b'\n' | b'e' => Some(AddonAction::Apply),
            b'j' => {
                self.selected = (self.selected + 1) % crate::addons::CATALOG.len();
                None
            }
            b'k' => {
                self.selected = (self.selected + crate::addons::CATALOG.len() - 1)
                    % crate::addons::CATALOG.len();
                None
            }
            _ => None,
        }
    }
    fn lines(
        &self,
        status: &Status,
        height: usize,
        installing: Option<&str>,
        notice: &str,
    ) -> Vec<String> {
        let count = height.saturating_sub(11).max(1);
        let start = self.selected.saturating_sub(count - 1);
        let mut out = vec!["SPOTIYA — ADD-ONS".into(), String::new()];
        for (index, addon) in crate::addons::CATALOG
            .iter()
            .enumerate()
            .skip(start)
            .take(count)
        {
            let label = if installing == Some(addon.id) {
                "Installing"
            } else {
                (addon.state)(status).label(addon.command.is_none())
            };
            out.push(format!(
                "{} {}   [{}]",
                if index == self.selected { ">" } else { " " },
                addon.name,
                label
            ));
        }
        let addon = &crate::addons::CATALOG[self.selected];
        let state = (addon.state)(status);
        out.push(String::new());
        out.push(addon.description.into());
        if installing == Some(addon.id) {
            out.push("Installation in progress. c: cancel download".into());
        } else if state.installed && addon.command.is_none() {
            out.push(
                "Ready; used automatically when ordinary Spotify audio keys are refused.".into(),
            );
        } else if state.installed {
            out.push(format!(
                "Enter: {} {}",
                if state.enabled { "disable" } else { "enable" },
                addon.name
            ));
        } else {
            out.push(format!(
                "Enter: download{} {}",
                if addon.command.is_some() {
                    " and enable"
                } else {
                    ""
                },
                addon.name
            ));
            out.push(format!(
                "GitHub Releases: klockky/spotiya v{}",
                env!("CARGO_PKG_VERSION")
            ));
        }
        if !state.error.is_empty() {
            out.push(safe(state.error));
        }
        out.push("Up/down or j/k: choose   Esc/q: back".into());
        out.push(String::new());
        out.extend(notice.lines().map(safe));
        out
    }
    fn draw(&self, status: &Status, installing: Option<&str>, notice: &str) -> io::Result<()> {
        let (width, height) = size();
        draw_lines(
            self.lines(status, height, installing, notice),
            width,
            height,
        )
    }
}
pub fn run(state_dir: &Path, show_addons: bool) -> io::Result<()> {
    use std::io::BufRead;
    let _terminal = Terminal::enter()?;
    let mut status = Status::default();
    let mut notice = "Ready. / searches Spotify; l pairs an account.".to_owned();
    let mut prompt: Option<Vec<u8>> = None;
    let mut profiles: Option<ProfileMenu> = None;
    let mut addons = show_addons.then(AddonMenu::default);
    let mut addon_job: Option<crate::addons::InstallJob> = None;
    let (addon_messages, addon_rx) = mpsc::channel();
    let mut addon_notice = String::new();
    let (updates, rx) = mpsc::channel::<String>();
    let mut login_socket: Option<std::os::unix::net::UnixStream> = None;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if addons.as_ref().is_some_and(|a| {
            a.escape != 0
                && a.escape_at
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(180))
        }) {
            addons = None;
        }
        if profiles
            .as_ref()
            .is_some_and(|p| p.escape != 0 && p.escape_at.elapsed() > Duration::from_millis(180))
        {
            profiles = None;
        }
        if let Ok(text) = fs::read_to_string(control::STATUS)
            && let Ok(fresh) = serde_json::from_str(&text)
        {
            status = fresh;
        }
        while let Ok(message) = rx.try_recv() {
            if message.starts_with("DONE") || message.starts_with("ERROR") {
                login_socket = None;
            }
            notice = message;
        }
        while let Ok(message) = addon_rx.try_recv() {
            match message {
                crate::addons::Message::Progress(text) => addon_notice = text,
                crate::addons::Message::Finished(result) => {
                    if let Some(job) = addon_job.take() {
                        addon_notice = match result {
                            Err(e) => e.to_string(),
                            Ok(()) => {
                                let addon = crate::addons::find(job.id)?;
                                match addon.command {
                                    None => format!("{} installed; ready on demand", addon.name),
                                    Some(command) => {
                                        match control::request(&format!("{command} ON")) {
                                            Ok(_) => {
                                                format!("{} installed; enabling...", addon.name)
                                            }
                                            Err(e) => format!(
                                                "Installed. Enable it after starting the receiver: {e}"
                                            ),
                                        }
                                    }
                                }
                            }
                        };
                    }
                }
            }
        }
        if last_draw.elapsed() >= Duration::from_millis(250) {
            if let Some(menu) = &addons {
                menu.draw(&status, addon_job.as_ref().map(|j| j.id), &addon_notice)?;
            } else if let Some(menu) = &profiles {
                menu.draw()?;
            } else {
                draw(&status, &notice, prompt.as_deref())?;
            }
            last_draw = Instant::now();
        }
        let mut input = [0u8; 64];
        let n = io::stdin().read(&mut input)?;
        for &key in &input[..n] {
            if key == 3 || key == 4 {
                if let Some(s) = login_socket.take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
                return Ok(());
            }
            if let Some(menu) = addons.as_mut() {
                match menu.key(key) {
                    Some(AddonAction::Cancel) => addons = None,
                    Some(AddonAction::StopInstall) => {
                        if let Some(job) = &mut addon_job {
                            job.cancel();
                            addon_notice = "Cancelling download...".into();
                        }
                    }
                    Some(AddonAction::Apply) => {
                        let addon = &crate::addons::CATALOG[menu.selected];
                        if addon_job.is_some() {
                            addon_notice = "An add-on installation is already running".into();
                        } else if (addon.state)(&status).installed {
                            addon_notice = match addon.command {
                                None => {
                                    format!("{} ready; used automatically when needed", addon.name)
                                }
                                Some(command) => match control::request(&format!(
                                    "{} {}",
                                    command,
                                    if (addon.state)(&status).enabled {
                                        "OFF"
                                    } else {
                                        "ON"
                                    }
                                )) {
                                    Ok(_) => "Command queued".into(),
                                    Err(e) => e.to_string(),
                                },
                            };
                        } else {
                            match crate::addons::InstallJob::start(addon, addon_messages.clone()) {
                                Ok(job) => {
                                    addon_job = Some(job);
                                    addon_notice = "Preparing add-on installation...".into();
                                }
                                Err(e) => addon_notice = e.to_string(),
                            }
                        }
                    }
                    None => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            if let Some(menu) = profiles.as_mut() {
                match menu.key(key) {
                    Some(ProfileAction::Cancel) => profiles = None,
                    Some(ProfileAction::Apply) => {
                        let choice = menu.entries[menu.selected].key().to_owned();
                        menu.notice = "Applying profile...".into();
                        menu.draw()?;
                        if let Some(socket) = login_socket.take() {
                            let _ = socket.shutdown(std::net::Shutdown::Both);
                        }
                        notice = match crate::profile_selection::select(state_dir, &choice) {
                            Ok(message) => message,
                            Err(e) => e.to_string(),
                        };
                        profiles = None;
                    }
                    None => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            if let Some(text) = prompt.as_mut() {
                match key {
                    27 => prompt = None,
                    127 | 8 => {
                        while let Some(byte) = text.pop() {
                            if byte & 0xc0 != 0x80 {
                                break;
                            }
                        }
                    }
                    b'\r' | b'\n' => {
                        let query = String::from_utf8_lossy(text).into_owned();
                        prompt = None;
                        notice = if query.trim().is_empty() {
                            "Search cancelled".into()
                        } else {
                            match control::request(&format!("PLAY {}", query.trim())) {
                                Ok(_) => "Spotify search queued".into(),
                                Err(e) => e.to_string(),
                            }
                        };
                    }
                    32..=255 if text.len() < 400 => text.push(key),
                    _ => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            let command = match key {
                b'q' => {
                    if let Some(s) = login_socket.take() {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                    return Ok(());
                }
                b' ' => Some("TOGGLE".to_owned()),
                b'n' => Some("NEXT".to_owned()),
                b'p' => Some("PREVIOUS".to_owned()),
                b'+' | b'=' => Some(format!("VOLUME {}", (status.volume + 5).min(100))),
                b'-' => Some(format!("VOLUME {}", status.volume.saturating_sub(5))),
                b'v' => {
                    if status.voice_supported {
                        Some(format!("VOICE {}", if status.voice { "OFF" } else { "ON" }))
                    } else {
                        notice = "Voice control is unavailable in this device profile".into();
                        None
                    }
                }
                b'e' => {
                    if status.spectrum_supported {
                        Some(format!(
                            "SPECTRUM {}",
                            if status.spectrum { "OFF" } else { "ON" }
                        ))
                    } else {
                        notice = "Spectrum display is unavailable on this device".into();
                        None
                    }
                }
                b'a' => {
                    addons = Some(AddonMenu::default());
                    None
                }
                b'r' => {
                    if status.retry_required && status.retry_after_secs == 0 {
                        Some("RETRY".into())
                    } else {
                        notice =
                            format!("Retry unavailable; cooldown {}s", status.retry_after_secs);
                        None
                    }
                }
                b'x' => Some("RELEASE".to_owned()),
                b'd' => {
                    match ProfileMenu::new(state_dir, &status.profile_selection) {
                        Ok(menu) => profiles = Some(menu),
                        Err(e) => notice = e.to_string(),
                    }
                    None
                }
                b'/' => {
                    prompt = Some(Vec::new());
                    None
                }
                b'l' => {
                    if login_socket.is_some() {
                        notice = "Login pending; open the link above. q cancels.".into();
                    } else {
                        match control::login() {
                            Ok(reader) => {
                                login_socket = Some(reader.get_ref().try_clone()?);
                                let tx = updates.clone();
                                std::thread::Builder::new()
                                    .name("pairing-ui".into())
                                    .stack_size(64 * 1024)
                                    .spawn(move || {
                                        for line in reader.lines() {
                                            match line {
                                                Ok(line) => {
                                                    let _ = tx.send(control::login_line(&line));
                                                }
                                                Err(_) => {
                                                    let _ = tx.send(
                                                        "ERROR Pairing connection closed".into(),
                                                    );
                                                    return;
                                                }
                                            }
                                        }
                                    })?;
                                notice = "Requesting a Spotify pairing code...".into();
                            }
                            Err(e) => notice = e.to_string(),
                        }
                    }
                    None
                }
                _ => None,
            };
            if let Some(command) = command {
                notice = match control::request(&command) {
                    Ok(_) => "Command queued".into(),
                    Err(e) => e.to_string(),
                };
            }
            last_draw = Instant::now() - Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addons_screen_shows_install_progress_and_service_states() {
        let mut menu = AddonMenu::default();
        let mut status = Status::default();
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s == "SPOTIYA — ADD-ONS"));
        assert!(
            frame
                .iter()
                .any(|s| s.contains("AirPlay 2   [Not installed]"))
        );
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Enter: download and enable"))
        );
        assert!(matches!(menu.key(b'\n'), Some(AddonAction::Apply)));
        let frame = menu.lines(&status, 24, Some("airplay"), "Downloading: 45%");
        assert!(frame.iter().any(|s| s.contains("[Installing]")));
        assert!(frame.iter().any(|s| s == "Downloading: 45%"));
        status.airplay_available = true;
        assert!(
            menu.lines(&status, 24, None, "")
                .iter()
                .any(|s| s.contains("[Off]"))
        );
        status.airplay_enabled = true;
        status.airplay_running = true;
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s.contains("[Running]")));
        assert!(frame.iter().any(|s| s == "Enter: disable AirPlay 2"));
        for key in b"\x1b[B" {
            assert!(menu.key(*key).is_none());
        }
        assert_eq!(menu.selected, 1);
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s.contains("Enter: download PlayPlay")));
        status.playplay_available = true;
        let frame = menu.lines(&status, 24, None, "");
        assert!(
            frame
                .iter()
                .any(|s| s.contains("PlayPlay   [Ready (on demand)]"))
        );
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Ready; used automatically"))
        );
        assert!(!frame.iter().any(|s| s.contains("Enter: disable PlayPlay")));
        assert!(matches!(menu.key(b'c'), Some(AddonAction::StopInstall)));
        assert!(matches!(menu.key(b'q'), Some(AddonAction::Cancel)));
    }
    #[test]
    fn profile_menu_navigation_scrolls_and_cancel_does_not_save() {
        let root = std::env::temp_dir().join(format!("spotiya-menu-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut menu = ProfileMenu::new(&root, "auto").unwrap();
        for key in b"\x1b[B\x1b[B\x1b[B" {
            assert!(menu.key(*key).is_none());
        }
        assert_eq!(menu.entries[menu.selected].key(), "generic");
        assert!(
            menu.lines(10)
                .iter()
                .any(|line| line.contains("> Generic ALSA"))
        );
        assert!(matches!(menu.key(b'q'), Some(ProfileAction::Cancel)));
        assert!(crate::profile_selection::read(&root).unwrap().is_none());
        menu.key(b'j');
        assert_eq!(menu.entries[menu.selected].key(), "auto");
        assert!(matches!(menu.key(b'\r'), Some(ProfileAction::Apply)));
        assert!(crate::profile_selection::read(&root).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn loading_and_failed_tracks_are_not_shown_as_playing() {
        let mut status = Status {
            connected: true,
            loading: true,
            ..Default::default()
        };
        assert!(
            lines(&status, "", None, 80)
                .iter()
                .any(|s| s.ends_with("|   LOADING"))
        );
        status.loading = false;
        status.error = "Track unavailable".into();
        let frame = lines(&status, "", None, 80);
        assert!(frame.iter().any(|s| s.ends_with("|   UNAVAILABLE")));
        assert!(frame.iter().any(|s| s == "Track unavailable"));
    }
    #[test]
    fn latched_failure_is_visible_while_spotify_is_disconnected() {
        let status = Status {
            retry_required: true,
            retry_after_secs: 12,
            error: "Audio key request failed".into(),
            ..Default::default()
        };
        let frame = lines(&status, "", None, 90);
        assert!(frame.iter().any(|s| s.ends_with("|   RETRY REQUIRED")));
        assert!(frame.iter().any(|s| s == "Retry available in 12s"));
        assert!(frame.iter().any(|s| s == "Audio key request failed"));
    }
    #[test]
    fn spectrum_state_is_visible_for_supported_and_other_devices() {
        let mut status = Status::default();
        assert!(
            lines(&status, "", None, 80)
                .iter()
                .any(|s| s == "Spectrum: unavailable")
        );
        status.spectrum_supported = true;
        for (enabled, label) in [(false, "off"), (true, "on")] {
            status.spectrum = enabled;
            let frame = lines(&status, "", None, 80);
            assert!(frame.iter().any(|s| s == &format!("Spectrum: {label}")));
            assert!(frame.iter().any(|s| s.contains("e spectrum on/off")));
        }
    }
    #[test]
    fn metadata_cannot_inject_terminal_controls_and_empty_progress_is_safe() {
        assert_eq!(safe("Song\x1b[2J\r\n"), "Song[2J");
        let mut s = Status {
            title: "\x1b[31mTitle".into(),
            ..Default::default()
        };
        assert!(lines(&s, "", None, 20).iter().all(|s| !s.contains('\x1b')));
        s.duration_ms = 100;
        s.position_ms = 999;
        let _ = lines(&s, "", None, 20);
    }
}
