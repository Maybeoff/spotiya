use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use librespot::{
    audio::playplay::PlayPlayDecoder,
    core::{Error, playplay::PlayPlayResolver},
};
use tokio::sync::OnceCell;

pub fn decoder_path(base: &Path) -> Option<PathBuf> {
    [
        base.join("playplay/bin/playplay-decoder"),
        base.join("playplay-decoder"),
    ]
    .into_iter()
    .find(|path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

pub fn available(base: &Path) -> bool {
    decoder_path(base).is_some()
}

pub fn resolver(base: PathBuf) -> PlayPlayResolver {
    let configuration = Arc::new(OnceCell::new());
    Arc::new(move || {
        let base = base.clone();
        let configuration = configuration.clone();
        Box::pin(async move {
            let config = configuration
                .get_or_try_init(|| async move {
                    let path = decoder_path(&base).ok_or_else(|| {
                        Error::unavailable(
                            "PlayPlay decoder missing; install PlayPlay from spotiya addons",
                        )
                    })?;
                    let config =
                        tokio::task::spawn_blocking(move || PlayPlayDecoder::configuration(&path))
                            .await
                            .map_err(|_| Error::aborted("PlayPlay configuration worker stopped"))?
                            .map_err(|error| {
                                Error::unavailable(format!(
                                    "PlayPlay decoder configuration failed ({:?})",
                                    error.kind()
                                ))
                            })?;
                    eprintln!("receiver: PlayPlay decoder configured on demand");
                    Ok::<_, Error>(config)
                })
                .await?;
            Ok(config.clone())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(reply: bool) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "spotiya-lazy-playplay-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let fixture = Self(directory);
            fixture.install(reply);
            fixture
        }
        fn install(&self, reply: bool) {
            let helper = self.helper();
            fs::create_dir_all(helper.parent().unwrap()).unwrap();
            let response = if reply {
                "printf '\\025\\000\\000\\000\\000\\005\\000\\000\\0000123456789abcdef'\n"
            } else {
                "exit 1\n"
            };
            // Copy in a child so no writable descriptor of the helper can leak into a
            // concurrent test's fork, which would make exec fail with ETXTBSY.
            let source = helper.with_extension("source");
            fs::write(
                &source,
                format!(
                    "#!/bin/sh\ndd bs=5 count=1 of=/dev/null 2>/dev/null\nprintf x >> \"$0.count\"\n{response}"
                ),
            )
            .unwrap();
            assert!(
                std::process::Command::new("cp")
                    .arg(&source)
                    .arg(&helper)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn helper(&self) -> PathBuf {
            self.0.join("playplay/bin/playplay-decoder")
        }
        fn calls(&self) -> usize {
            fs::read(self.helper().with_extension("count")).map_or(0, |value| value.len())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn helper_starts_lazily_once_and_configuration_survives_concurrent_callers() {
        let fixture = Fixture::new(true);
        let resolve = resolver(fixture.0.clone());
        assert_eq!(fixture.calls(), 0);
        let (first, second) = tokio::join!(resolve(), resolve());
        first.unwrap();
        second.unwrap();
        resolve().await.unwrap();
        assert_eq!(fixture.calls(), 1);
    }

    #[tokio::test]
    async fn failed_probe_is_retryable() {
        let fixture = Fixture::new(false);
        let resolve = resolver(fixture.0.clone());
        assert!(resolve().await.is_err());
        assert!(resolve().await.is_err());
        assert_eq!(fixture.calls(), 2);
    }
    #[tokio::test]
    async fn addon_installed_after_startup_is_used_without_a_restart() {
        let fixture = Fixture::new(true);
        fs::remove_file(fixture.helper()).unwrap();
        let resolve = resolver(fixture.0.clone());
        assert!(!available(&fixture.0));
        assert!(
            resolve()
                .await
                .unwrap_err()
                .to_string()
                .contains("spotiya addons")
        );
        assert_eq!(fixture.calls(), 0);
        fixture.install(true);
        assert!(available(&fixture.0));
        resolve().await.unwrap();
        assert_eq!(fixture.calls(), 1);
    }

    #[test]
    fn prefers_addon_but_preserves_legacy_manual_installs() {
        let fixture = Fixture::new(true);
        let legacy = fixture.0.join("playplay-decoder");
        fs::copy(fixture.helper(), &legacy).unwrap();
        assert_eq!(decoder_path(&fixture.0), Some(fixture.helper()));
        fs::set_permissions(fixture.helper(), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(decoder_path(&fixture.0), Some(legacy));
    }
}
