use librespot::playback::player::PlayerEvent;
use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};
use tokio::time::Instant;

const LIMIT: u8 = 3;
const COOLDOWN: Duration = Duration::from_secs(15);
static LAST_ERROR: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Reason {
    #[default]
    Unknown,
    Decoder,
    Network,
    AudioKey,
    AudioDevice,
}
impl Reason {
    fn classify(target: &str, message: &str) -> Self {
        if target == "librespot_core::audio_key" {
            Self::AudioKey
        } else if target.starts_with("librespot_playback::audio_backend")
            || message.contains("ALSA")
            || message.contains("Alsa")
        {
            Self::AudioDevice
        } else if target.starts_with("librespot_core::http_client")
            || target.starts_with("librespot_core::cdn_url")
            || target.starts_with("librespot_audio::fetch")
        {
            Self::Network
        } else if message.contains("Decoder") || message.contains("decod") {
            Self::Decoder
        } else {
            Self::Unknown
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::AudioKey => "Spotify refused or could not provide an audio key",
            Self::AudioDevice => "Audio device failed",
            Self::Network => "Audio download or network request failed",
            Self::Decoder => "Audio decoding failed",
            Self::Unknown => "Tracks could not be loaded; see spotify-connect.log",
        }
    }
}
pub fn note(record: &log::Record<'_>) {
    if record.level() > log::Level::Warn || !record.target().starts_with("librespot_") {
        return;
    }
    let reason = Reason::classify(record.target(), &record.args().to_string());
    // Preserve the cause when a missing key subsequently produces decoder errors.
    LAST_ERROR.fetch_max(reason as u8, Ordering::Relaxed);
}
pub fn clear_diagnostics() {
    LAST_ERROR.store(0, Ordering::Relaxed);
}
fn last_reason() -> Reason {
    match LAST_ERROR.load(Ordering::Relaxed) {
        1 => Reason::Decoder,
        2 => Reason::Network,
        3 => Reason::AudioKey,
        4 => Reason::AudioDevice,
        _ => Reason::Unknown,
    }
}

#[derive(Default)]
pub struct Failures {
    count: u8,
    last_request: Option<u64>,
    track: Option<String>,
    blocked_until: Option<Instant>,
    reason: Reason,
}
impl Failures {
    pub fn reset(&mut self) {
        *self = Self::default();
        clear_diagnostics();
    }
    pub fn blocked(&self) -> bool {
        self.blocked_until.is_some()
    }
    pub fn count(&self) -> u8 {
        self.count
    }
    pub fn retry_after(&self) -> u32 {
        self.blocked_until.map_or(0, |until| {
            until
                .saturating_duration_since(Instant::now())
                .as_secs()
                .saturating_add(u64::from(until > Instant::now()))
        }) as u32
    }
    pub fn message(&self) -> String {
        if self.blocked() {
            format!(
                "Stopped after {LIMIT} failed tracks. {}",
                self.reason.message()
            )
        } else {
            String::new()
        }
    }
    // Call only for events accepted by Playback, excluding failed preloads and stale requests.
    pub fn observe(&mut self, event: &PlayerEvent) -> bool {
        if self.blocked() {
            return false;
        }
        match event {
            PlayerEvent::Playing { .. } | PlayerEvent::Paused { .. } => self.reset(),
            PlayerEvent::Unavailable {
                play_request_id,
                track_id,
            } => {
                if self.last_request == Some(*play_request_id) {
                    return false;
                }
                self.last_request = Some(*play_request_id);
                self.track = track_id.to_uri().ok();
                self.count = self.count.saturating_add(1);
                self.reason = last_reason();
                if self.count >= LIMIT {
                    self.blocked_until = Some(Instant::now() + COOLDOWN);
                    return true;
                }
            }
            _ => {}
        }
        false
    }
    pub fn retry(&mut self) -> Option<String> {
        if !self.blocked() || self.retry_after() != 0 {
            return None;
        }
        let track = self.track.take()?;
        self.reset();
        Some(track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot::core::SpotifyUri;
    fn unavailable(request: u64) -> PlayerEvent {
        PlayerEvent::Unavailable {
            play_request_id: request,
            track_id: SpotifyUri::from_uri("spotify:track:0000000000000000000001").unwrap(),
        }
    }
    #[tokio::test(start_paused = true)]
    async fn stops_after_three_distinct_failed_requests_and_requires_explicit_retry() {
        let mut failures = Failures::default();
        assert!(!failures.observe(&unavailable(1)));
        assert!(!failures.observe(&unavailable(1)));
        assert_eq!(failures.count(), 1);
        assert!(!failures.observe(&unavailable(2)));
        assert!(failures.observe(&unavailable(3)));
        assert!(failures.blocked());
        assert!(failures.retry().is_none());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(failures.blocked());
        assert_eq!(failures.retry_after(), 0);
        assert!(failures.retry().unwrap().starts_with("spotify:track:"));
        assert!(!failures.blocked());
        assert_eq!(failures.count(), 0);
    }
    #[test]
    fn preload_errors_are_excluded_by_the_playback_state() {
        let mut playback = crate::playback::Playback::default();
        let mut failures = Failures::default();
        let track = SpotifyUri::from_uri("spotify:track:0000000000000000000001").unwrap();
        let other = SpotifyUri::from_uri("spotify:track:0000000000000000000002").unwrap();
        for request in 1..=3 {
            for event in [
                PlayerEvent::PlayRequestIdChanged {
                    play_request_id: request,
                },
                PlayerEvent::Loading {
                    play_request_id: request,
                    track_id: track.clone(),
                    position_ms: 0,
                },
                PlayerEvent::Unavailable {
                    play_request_id: request,
                    track_id: other.clone(),
                },
                PlayerEvent::Unavailable {
                    play_request_id: request,
                    track_id: track.clone(),
                },
            ] {
                if playback.update(&event) {
                    failures.observe(&event);
                }
            }
            assert_eq!(failures.count(), request as u8);
        }
        assert!(failures.blocked());
    }
    #[test]
    fn successful_load_breaks_the_failure_streak() {
        let mut failures = Failures::default();
        failures.observe(&unavailable(1));
        failures.observe(&unavailable(2));
        if let PlayerEvent::Unavailable { track_id, .. } = unavailable(2) {
            failures.observe(&PlayerEvent::Playing {
                play_request_id: 2,
                track_id,
                position_ms: 0,
            });
        }
        assert_eq!(failures.count(), 0);
        assert!(!failures.observe(&unavailable(3)));
    }
    #[test]
    fn diagnostics_classify_causes_without_storing_raw_error_strings() {
        assert_eq!(
            Reason::classify("librespot_core::audio_key", "error audio key 0 1"),
            Reason::AudioKey
        );
        assert_eq!(
            Reason::classify("librespot_playback::audio_backend::alsa", "failed"),
            Reason::AudioDevice
        );
        assert_eq!(
            Reason::classify("librespot_audio::fetch", "failed"),
            Reason::Network
        );
        assert_eq!(
            Reason::classify("librespot_playback::player", "Symphonia Decoder Error"),
            Reason::Decoder
        );
        assert_eq!(
            Reason::classify("librespot_playback::player", "other"),
            Reason::Unknown
        );
    }
}
