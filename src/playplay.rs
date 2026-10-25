use std::{path::PathBuf, sync::Arc};

use librespot::{
    audio::playplay::PlayPlayDecoder,
    core::{Error, playplay::PlayPlayResolver},
};
use tokio::sync::OnceCell;

pub fn resolver(path: PathBuf) -> Option<PlayPlayResolver> {
    if !path.is_file() {
        return None;
    }
    let configuration = Arc::new(OnceCell::new());
    Some(Arc::new(move || {
        let path = path.clone();
        let configuration = configuration.clone();
        Box::pin(async move {
            let config = configuration
                .get_or_try_init(|| async move {
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
    }))
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
            let helper = directory.join("decoder");
            let response = if reply {
                "printf '\\025\\000\\000\\000\\000\\005\\000\\000\\0000123456789abcdef'\n"
            } else {
                "exit 1\n"
            };
            fs::write(
                &helper,
                format!(
                    "#!/bin/sh\ndd bs=5 count=1 of=/dev/null 2>/dev/null\nprintf x >> \"$0.count\"\n{response}"
                ),
            )
            .unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            Self(directory)
        }
        fn helper(&self) -> PathBuf {
            self.0.join("decoder")
        }
        fn calls(&self) -> usize {
            fs::read(self.0.join("decoder.count")).map_or(0, |value| value.len())
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
        let resolve = resolver(fixture.helper()).unwrap();
        assert_eq!(fixture.calls(), 0);
        let (first, second) = tokio::join!(resolve(), resolve());
        first.unwrap();
        second.unwrap();
        resolve().await.unwrap();
        assert_eq!(fixture.calls(), 1);
    }

    #[tokio::test]
    async fn failed_probe_is_retryable_and_missing_helper_has_no_resolver() {
        let fixture = Fixture::new(false);
        let resolve = resolver(fixture.helper()).unwrap();
        assert!(resolve().await.is_err());
        assert!(resolve().await.is_err());
        assert_eq!(fixture.calls(), 2);
        assert!(resolver(fixture.0.join("missing")).is_none());
    }
}
