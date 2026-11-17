//! Keeps the model lists current while kage runs: the curated catalog,
//! the `ChatGPT` plan's list with a login, and the lists of custom
//! providers that name a `models_url`.
//!
//! A host fetches each list once `[models] refresh_interval_hours` have
//! passed since its cache was written or last found current, by this
//! process or another. Every long-lived process, hosts and TUIs alike,
//! applies a list whose cache changed, whoever wrote it.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use kage_core::config::Config;
use kage_provider::catalog::{self, Refreshed};

use crate::rpc::host::Host;
use crate::serve::Log;

/// How long after start the first check waits, so it never slows
/// startup and a short-lived process never fetches.
const FIRST_CHECK: Duration = Duration::from_secs(30);

/// How often a process looks for lists another process refreshed, and
/// the longest it sleeps, so a changed `[models]` section or a newly
/// configured provider applies within it.
const WAKE: Duration = Duration::from_secs(15 * 60);

/// How long a list whose refresh failed waits before the next try.
const RETRY: Duration = Duration::from_secs(3600);

/// One model list kage keeps current.
enum Source {
    Catalog,
    ChatGpt,
    /// A custom provider by id, with the `models_url` it lists from.
    Custom(String, String),
}

impl Source {
    /// Every list `config` asks to keep current.
    fn all(config: &Config) -> Vec<Self> {
        let custom = crate::custom_models::listed_providers(config)
            .map(|(id, url)| Self::Custom(id.to_owned(), url.to_owned()));
        [Self::Catalog, Self::ChatGpt]
            .into_iter()
            .chain(custom)
            .collect()
    }

    fn name(&self) -> String {
        match self {
            Self::Catalog => "model catalog".to_owned(),
            Self::ChatGpt => "ChatGPT plan models".to_owned(),
            Self::Custom(id, _) => format!("{id} models"),
        }
    }

    /// The bytes of the list's cache, to tell whether it changed. The
    /// catalog is told by the digest it was laid from instead.
    fn body(&self) -> Option<Vec<u8>> {
        let path = match self {
            Self::Catalog => return None,
            Self::ChatGpt => crate::chatgpt::models_cache_path(),
            Self::Custom(id, _) => crate::custom_models::cache_path(id),
        };
        std::fs::read(path.ok()?).ok()
    }

    /// When the list was cached or last found current, `None` without
    /// a cache of it that reads, so a broken one is fetched again.
    fn written(&self) -> Option<SystemTime> {
        let path = match self {
            Self::Catalog => {
                // The body in effect parsed when it was laid; only another
                // one is decoded to tell whether it reads.
                let path = crate::paths::models_cache_path().ok()?;
                let body = std::fs::read_to_string(&path).ok()?;
                if catalog::loaded_digest() != Some(digest(body.as_bytes())) {
                    catalog::source::parse(&body).ok()?;
                }
                path
            }
            Self::ChatGpt => {
                let path = crate::chatgpt::models_cache_path().ok()?;
                if crate::chatgpt::cached_models(&path).is_empty() {
                    return None;
                }
                path
            }
            Self::Custom(id, url) => return crate::custom_models::written(id, url),
        };
        std::fs::metadata(path).ok()?.modified().ok()
    }

    /// Fetches the list into its cache.
    fn fetch(&self, config: &Config) -> Result<(), String> {
        match self {
            Self::Catalog => {
                let path = crate::paths::models_cache_path()?;
                catalog::refresh_if_changed(&catalog::source::catalog_url(), &path)
                    .map(|_: Refreshed| ())
            }
            Self::ChatGpt => crate::chatgpt::refresh_plan_models().map(|_| ()),
            Self::Custom(id, _) => {
                let store = crate::auth::AuthStore::load()?;
                crate::custom_models::refresh(id, config, &store).map(|_| ())
            }
        }
    }
}

/// Starts keeping `host`'s model lists current, for as long as it runs:
/// fetching due lists, and reloading its providers when one changed.
/// `applied` is what the host's providers were built from, taken before
/// they were. Refreshes and failures go to `log`; a TUI serving its own
/// host passes none, since its terminal is the screen.
pub(crate) fn spawn_host(host: &Arc<Host>, log: Option<Log>, applied: Applied) {
    let host = Arc::downgrade(host);
    let _ = thread::Builder::new()
        .name("kage-model-refresh".to_owned())
        .spawn(move || run(&mut Role::Host(host, log), applied));
}

/// What a watcher's rebuild came to.
pub(crate) enum Rebuilt {
    /// It shows the new lists.
    Done,
    /// It could not, as with a broken config; the next wake tries again.
    Failed,
    /// Nothing is left to show them to.
    Gone,
}

/// Starts applying the model lists a host refreshed to this process:
/// `rebuild` remakes what it shows from them. `applied` is what it shows
/// now, taken before that was built.
pub(crate) fn spawn_watcher(applied: Applied, rebuild: impl FnMut() -> Rebuilt + Send + 'static) {
    let _ = thread::Builder::new()
        .name("kage-model-watch".to_owned())
        .spawn(move || run(&mut Role::Watcher(Box::new(rebuild)), applied));
}

/// What a process does with the model lists.
enum Role {
    /// Fetches them for this host, and reloads its providers.
    Host(Weak<Host>, Option<Log>),
    /// Applies what a host fetched.
    Watcher(Box<dyn FnMut() -> Rebuilt + Send>),
}

/// The digest of each list's cache as a refresher last applied it.
pub(crate) struct Applied(HashMap<String, [u8; 32]>);

impl Applied {
    /// The lists as they are now. Taken before the providers are built
    /// from them, so a list written in between is applied once more
    /// rather than missed.
    pub(crate) fn now() -> Self {
        let config = Config::load_default_raw().unwrap_or_default();
        let mut applied = Self(HashMap::new());
        for source in Source::all(&config) {
            let current = if matches!(source, Source::Catalog) {
                catalog::loaded_digest()
            } else {
                source.body().map(|body| digest(&body))
            };
            if let Some(current) = current {
                applied.0.insert(source.name(), current);
            }
        }
        applied
    }

    /// `source`'s name and cache digest when the cache differs from the
    /// one this role last applied. The catalog is also laid over the
    /// snapshot then, once per change in the process however many
    /// roles see it.
    fn changed(&self, source: &Source) -> Option<(String, [u8; 32])> {
        let name = source.name();
        let digest = if matches!(source, Source::Catalog) {
            // The catalog in effect is the process's own: lay a changed
            // cache over it, then compare what is in effect, without
            // reading the file again.
            let path = crate::paths::models_cache_path().ok()?;
            let _ = catalog::use_cache_if_changed(&path);
            catalog::loaded_digest()?
        } else {
            digest(&source.body()?)
        };
        (self.0.get(&name) != Some(&digest)).then_some((name, digest))
    }
}

/// The SHA-256 digest of `bytes`.
fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

fn run(role: &mut Role, mut applied: Applied) {
    thread::sleep(FIRST_CHECK);
    // When each list that failed may be tried again; a list fetched
    // fine is due from its cache's age under the current interval.
    let mut retry_at: HashMap<String, SystemTime> = HashMap::new();
    let mut failing: HashMap<String, bool> = HashMap::new();
    loop {
        if let Role::Host(host, _) = role
            && host.strong_count() == 0
        {
            return;
        }
        // A config that does not load fetches nothing, since it may be
        // the one turning refreshes off; lists others wrote still apply.
        let loaded = Config::load_default_raw().ok();
        let fetching = loaded
            .as_ref()
            .is_some_and(|config| config.models.auto_refresh);
        let config = loaded.unwrap_or_default();
        let interval = Duration::from_secs(
            config
                .models
                .refresh_interval_hours
                .clamp(1, kage_core::config::MAX_REFRESH_INTERVAL_HOURS)
                * 3600,
        );
        let mut sleep = WAKE;
        let mut pending = Vec::new();
        for source in Source::all(&config) {
            let name = source.name();
            if let Role::Host(_, log) = role
                && fetching
            {
                let now = SystemTime::now();
                let left = wait(
                    source.written(),
                    retry_at
                        .get(&name)
                        .copied()
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                    now,
                    interval,
                );
                if left.is_zero() {
                    let failed = source.fetch(&config).err();
                    let was_failing = failing.insert(name.clone(), failed.is_some());
                    // A failure is told once, not on every retry.
                    if let (Some(error), Some(log)) = (&failed, log.as_ref())
                        && was_failing != Some(true)
                    {
                        log(&format!("{name} refresh failed: {error}"));
                    }
                    // A fetch that left no cache to date the next one by, as
                    // a plan listing no usable model, waits an interval.
                    let next = match (&failed, source.written()) {
                        (Some(_), _) => Some(now + RETRY),
                        (None, None) => Some(now + interval),
                        (None, Some(_)) => None,
                    };
                    match next {
                        Some(at) => retry_at.insert(name.clone(), at),
                        None => retry_at.remove(&name),
                    };
                } else {
                    sleep = sleep.min(left);
                }
            }
            pending.extend(applied.changed(&source));
        }
        // A list counts as applied once the reload or rebuild took it,
        // so one a broken config kept out is tried again.
        let took = pending.is_empty()
            || match role {
                Role::Host(host, log) => match host.upgrade().map(|host| host.reload_providers()) {
                    Some(Ok(())) => {
                        if let Some(log) = log.as_ref() {
                            for (name, _) in &pending {
                                log(&format!("{name} updated"));
                            }
                        }
                        true
                    }
                    Some(Err(error)) => {
                        if let Some(log) = log.as_ref() {
                            log(&format!("cannot reload the providers: {error}"));
                        }
                        false
                    }
                    None => return,
                },
                Role::Watcher(rebuild) => match rebuild() {
                    Rebuilt::Done => true,
                    Rebuilt::Failed => false,
                    Rebuilt::Gone => return,
                },
            };
        if took {
            applied.0.extend(pending);
        }
        thread::sleep(sleep);
    }
}

/// How long until a list is due: `interval` after its cache was
/// written (`cached`, `None` when there is none), and not before
/// `not_before`, when a failed try may be repeated.
fn wait(
    cached: Option<SystemTime>,
    not_before: SystemTime,
    now: SystemTime,
    interval: Duration,
) -> Duration {
    let due = cached.map_or(SystemTime::UNIX_EPOCH, |at| at + interval);
    due.max(not_before)
        .duration_since(now)
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn a_list_is_due_an_interval_after_its_cache_was_written() {
        let now = SystemTime::now();
        let never = SystemTime::UNIX_EPOCH;
        assert_eq!(wait(None, never, now, 24 * HOUR), Duration::ZERO);
        assert_eq!(
            wait(Some(now - 25 * HOUR), never, now, 24 * HOUR),
            Duration::ZERO
        );
        assert_eq!(wait(Some(now - 20 * HOUR), never, now, 24 * HOUR), 4 * HOUR);
    }

    #[test]
    fn a_failed_or_empty_check_waits_for_its_own_retry() {
        let now = SystemTime::now();
        assert_eq!(wait(None, now + HOUR, now, 24 * HOUR), HOUR);
        assert_eq!(
            wait(Some(now - 25 * HOUR), now + HOUR, now, 24 * HOUR),
            HOUR,
            "a stale cache whose refresh failed is not tried at once"
        );
    }
}
