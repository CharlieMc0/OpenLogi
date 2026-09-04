//! Built-in asset mirror discovery and npm shard routing.
//!
//! A synchronization run races the production custom domain, its versioned
//! Cloudflare Pages alias, and the fixed jsDelivr npm release. Whichever
//! source answers supplies both `index.json` and every subsequent file URL
//! for that run, so caches never mix mirrors mid-sync — except Production
//! wins over a same-race pinned-mirror answer as long as it reports in
//! within [`PRODUCTION_GRACE`]: Pages and jsDelivr are frozen at
//! `ASSET_VERSION`, so letting either win a pure speed race would let a
//! stale catalog silently beat a healthy, more current Production.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::error::AssetError;
use crate::http::{AssetClient, write_replace};
use crate::index::{INDEX_NAME, Index};

/// Mutable production endpoint behind the OpenLogi custom domain.
const PRODUCTION_BASE: &str = "https://assets.openlogi.org";

/// Stable Cloudflare Pages branch alias for asset release 0.1.0.
const PAGES_BASE: &str = "https://v0-1-0.openlogi-assets.pages.dev";

/// Exact jsDelivr catalog package for asset release 0.1.0.
const JSDELIVR_CATALOG_BASE: &str = "https://cdn.jsdelivr.net/npm/@logi-assets/catalog@0.1.0";

/// jsDelivr prefix shared by every npm asset shard.
const JSDELIVR_PACKAGE_ROOT: &str = "https://cdn.jsdelivr.net/npm";

/// npm asset release this OpenLogi build understands.
const ASSET_VERSION: &str = "0.1.0";

/// How long a pinned-version mirror's success waits for the mutable
/// Production endpoint before it is used.
///
/// Pages and jsDelivr are frozen at `ASSET_VERSION` — potentially months
/// behind Production, which publishes new depots continuously — but their
/// edge caches routinely answer faster than Production's custom domain. This
/// window absorbs that ordinary latency gap while staying short enough that
/// a genuine Production outage still falls back close to instantly.
const PRODUCTION_GRACE: Duration = Duration::from_millis(1_200);

/// Filename and schema of the depot-to-package routing catalog.
const NPM_ROUTES_NAME: &str = "npm-routes.json";
const NPM_ROUTES_SCHEMA: u32 = 1;

/// Asset endpoint selected for one synchronization run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetSource {
    /// Mutable production custom domain.
    Production,
    /// Versioned Cloudflare Pages branch alias matching the npm release.
    Pages,
    /// Versioned npm packages served through jsDelivr.
    JsDelivr,
    /// Explicit `OPENLOGI_ASSETS` or CLI `--base` override.
    Override(String),
}

#[derive(Clone, Copy, Debug)]
enum BuiltInSource {
    Production,
    Pages,
    JsDelivr,
}

impl From<BuiltInSource> for AssetSource {
    fn from(source: BuiltInSource) -> Self {
        match source {
            BuiltInSource::Production => Self::Production,
            BuiltInSource::Pages => Self::Pages,
            BuiltInSource::JsDelivr => Self::JsDelivr,
        }
    }
}

impl fmt::Display for BuiltInSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        AssetSource::from(*self).fmt(formatter)
    }
}

impl fmt::Display for AssetSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Production => formatter.write_str(PRODUCTION_BASE),
            Self::Pages => formatter.write_str(PAGES_BASE),
            Self::JsDelivr => formatter.write_str(JSDELIVR_CATALOG_BASE),
            Self::Override(base) => formatter.write_str(base),
        }
    }
}

/// A parsed registry and the client pinned to the mirror that supplied it.
pub struct AssetRegistry {
    client: AssetClient,
    index: Index,
    source: AssetSource,
}

impl AssetRegistry {
    /// Load a registry into `dir`.
    ///
    /// An explicit `base` uses that uniform origin. Without an override, the
    /// production domain, versioned Pages alias, and versioned jsDelivr
    /// mirror are probed concurrently; the first complete valid catalog wins.
    pub fn load(base: Option<&str>, dir: &Path) -> Result<Self, AssetError> {
        Self::load_source(base.map(|base| AssetSource::Override(base.to_owned())), dir)
    }

    /// Load a registry from one selected built-in source, or race all built-in
    /// sources when `source` is `None`.
    ///
    /// Unlike passing the jsDelivr catalog URL as a uniform override to
    /// [`Self::load`], selecting [`AssetSource::JsDelivr`] also loads and
    /// validates its npm shard routes.
    pub fn load_source(source: Option<AssetSource>, dir: &Path) -> Result<Self, AssetError> {
        match source {
            Some(source) => load_selected_source(source, dir),
            None => probe_default_sources(dir),
        }
    }

    /// Client pinned to the selected mirror for the lifetime of this registry.
    #[must_use]
    pub fn client(&self) -> &AssetClient {
        &self.client
    }

    /// Parsed `index.json` returned by the selected mirror.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Mirror selected by the source probes.
    #[must_use]
    pub fn source(&self) -> &AssetSource {
        &self.source
    }
}

#[derive(Deserialize)]
struct NpmRoutes {
    schema_version: u32,
    package_version: String,
    devices: HashMap<String, String>,
}

struct ProbeSuccess {
    client: AssetClient,
    index_bytes: Vec<u8>,
    index: Index,
}

fn load_selected_source(source: AssetSource, dir: &Path) -> Result<AssetRegistry, AssetError> {
    let probe = match &source {
        AssetSource::Production => probe_uniform(PRODUCTION_BASE),
        AssetSource::Pages => probe_uniform(PAGES_BASE),
        AssetSource::JsDelivr => probe_jsdelivr(),
        AssetSource::Override(base) => probe_uniform(base),
    }?;
    registry_from_probe(source, probe, dir)
}

fn registry_from_probe(
    source: AssetSource,
    probe: ProbeSuccess,
    dir: &Path,
) -> Result<AssetRegistry, AssetError> {
    write_replace(&dir.join(INDEX_NAME), &probe.index_bytes)?;
    info!(%source, "asset source selected");
    Ok(AssetRegistry {
        client: probe.client,
        index: probe.index,
        source,
    })
}

fn probe_default_sources(dir: &Path) -> Result<AssetRegistry, AssetError> {
    let (sender, receiver) = mpsc::channel();

    for (source, base) in [
        (BuiltInSource::Production, PRODUCTION_BASE),
        (BuiltInSource::Pages, PAGES_BASE),
    ] {
        let sender = sender.clone();
        thread::spawn(move || {
            let result = probe_uniform(base);
            if sender.send((source, result)).is_err() {
                debug!(%source, "asset source probe finished after a winner was selected");
            }
        });
    }
    let jsdelivr_sender = sender.clone();
    thread::spawn(move || {
        let result = probe_jsdelivr();
        if jsdelivr_sender
            .send((BuiltInSource::JsDelivr, result))
            .is_err()
        {
            debug!("jsDelivr probe finished after a winner was selected");
        }
    });
    drop(sender);

    match race_sources(&receiver, PRODUCTION_GRACE) {
        RaceOutcome::Use(source, probe) => registry_from_probe(source.into(), probe, dir),
        RaceOutcome::AllFailed {
            production,
            pages,
            jsdelivr,
        } => Err(AssetError::SourcesUnavailable {
            production,
            pages,
            jsdelivr,
        }),
        RaceOutcome::Interrupted => Err(AssetError::SourceProbeInterrupted),
    }
}

/// Result of a [`MirrorRace`]: which source answered, or why none did.
enum RaceOutcome<T> {
    /// This source's probe succeeded and should supply the registry.
    Use(BuiltInSource, T),
    /// Every built-in source's probe failed.
    AllFailed {
        production: Box<AssetError>,
        pages: Box<AssetError>,
        jsdelivr: Box<AssetError>,
    },
    /// The channel closed before a winner was selected — the sending
    /// threads panicked or were otherwise dropped without reporting.
    Interrupted,
}

/// What the owning loop should do after feeding [`MirrorRace`] one event.
enum Step<T> {
    /// The race is decided — use this outcome.
    Done(RaceOutcome<T>),
    /// Keep waiting; recompute the receive timeout from
    /// [`MirrorRace::deadline`]. Carries the source of a second pinned-mirror
    /// success arriving while one was already held, purely so the loop can
    /// log it — nothing else about the race changes.
    Continue { superseded: Option<BuiltInSource> },
}

/// Pure decision state for arbitrating [`BuiltInSource`] probe results.
///
/// Holds no I/O: the owning loop drives the actual [`mpsc::Receiver`] waits
/// and feeds each event here with an explicit `now`, so every arrival-order
/// combination is a plain, deterministic method call under test — no
/// wall-clock sleeps standing in for real race timing. Production wins
/// immediately; a pinned mirror (Pages or jsDelivr) answering first only
/// wins once either Production has also failed, or `grace` has elapsed
/// without Production answering — see [`PRODUCTION_GRACE`] for why an
/// unconditional speed race is wrong here.
struct MirrorRace<T> {
    production_error: Option<AssetError>,
    pages_error: Option<AssetError>,
    jsdelivr_error: Option<AssetError>,
    pinned_fallback: Option<(BuiltInSource, T)>,
    deadline: Option<Instant>,
}

impl<T> MirrorRace<T> {
    fn new() -> Self {
        Self {
            production_error: None,
            pages_error: None,
            jsdelivr_error: None,
            pinned_fallback: None,
            deadline: None,
        }
    }

    /// How long the owning loop should wait for the next event — `None`
    /// means block indefinitely (no pinned fallback is on the clock yet).
    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Record one probe success.
    fn on_success(
        &mut self,
        source: BuiltInSource,
        probe: T,
        now: Instant,
        grace: Duration,
    ) -> Step<T> {
        if matches!(source, BuiltInSource::Production) {
            return Step::Done(RaceOutcome::Use(source, probe));
        }
        if self.production_error.is_some() {
            // Production is already known dead — nothing left to wait for.
            return Step::Done(RaceOutcome::Use(source, probe));
        }
        if self.pinned_fallback.is_some() {
            return Step::Continue {
                superseded: Some(source),
            };
        }
        self.pinned_fallback = Some((source, probe));
        self.deadline = Some(now + grace);
        Step::Continue { superseded: None }
    }

    /// Record one probe failure.
    fn on_failure(&mut self, source: BuiltInSource, error: AssetError) -> Step<T> {
        match source {
            BuiltInSource::Production => {
                self.production_error = Some(error);
                // Production is now confirmed dead: a pinned fallback
                // already in hand has nothing left to wait for.
                if let Some((source, probe)) = self.pinned_fallback.take() {
                    return Step::Done(RaceOutcome::Use(source, probe));
                }
            }
            BuiltInSource::Pages => self.pages_error = Some(error),
            BuiltInSource::JsDelivr => self.jsdelivr_error = Some(error),
        }
        match (
            self.production_error.take(),
            self.pages_error.take(),
            self.jsdelivr_error.take(),
        ) {
            (Some(production), Some(pages), Some(jsdelivr)) => Step::Done(RaceOutcome::AllFailed {
                production: Box::new(production),
                pages: Box::new(pages),
                jsdelivr: Box::new(jsdelivr),
            }),
            // Not every source has reported yet — put the errors collected
            // so far back for the next call to see.
            (production, pages, jsdelivr) => {
                self.production_error = production;
                self.pages_error = pages;
                self.jsdelivr_error = jsdelivr;
                Step::Continue { superseded: None }
            }
        }
    }

    /// The receive wait ended with no more sources left to hear from
    /// (deadline elapsed, or the channel disconnected) — use whatever
    /// pinned fallback is on hand, or give up.
    fn conclude(&mut self) -> RaceOutcome<T> {
        self.pinned_fallback
            .take()
            .map_or(RaceOutcome::Interrupted, |(source, probe)| {
                RaceOutcome::Use(source, probe)
            })
    }
}

/// Arbitrate the three probe threads' results — see [`MirrorRace`].
fn race_sources<T>(
    receiver: &mpsc::Receiver<(BuiltInSource, Result<T, AssetError>)>,
    grace: Duration,
) -> RaceOutcome<T> {
    let mut race = MirrorRace::new();
    loop {
        let event = match race.deadline() {
            Some(deadline) => {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let (source, result) = match event {
            Ok(pair) => pair,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                return race.conclude();
            }
        };

        let step = match result {
            Ok(probe) => race.on_success(source, probe, Instant::now(), grace),
            Err(error) => {
                warn!(%source, error = ?error, "asset source probe failed");
                race.on_failure(source, error)
            }
        };
        match step {
            Step::Done(outcome) => return outcome,
            Step::Continue { superseded: None } => {}
            Step::Continue {
                superseded: Some(source),
            } => {
                debug!(
                    %source,
                    "asset source probe answered after a pinned fallback was already recorded"
                );
            }
        }
    }
}

fn probe_uniform(base: &str) -> Result<ProbeSuccess, AssetError> {
    let client = AssetClient::new(base);
    let (index_bytes, index) = client.fetch_index_raw()?;
    Ok(ProbeSuccess {
        client,
        index_bytes,
        index,
    })
}

fn probe_jsdelivr() -> Result<ProbeSuccess, AssetError> {
    let catalog_client = AssetClient::new(JSDELIVR_CATALOG_BASE);
    let (index_bytes, index) = catalog_client.fetch_index_raw()?;
    let routes_url = format!("{JSDELIVR_CATALOG_BASE}/{NPM_ROUTES_NAME}");
    let routes_bytes = catalog_client.get_bytes(&routes_url)?;
    let routes: NpmRoutes =
        serde_json::from_slice(&routes_bytes).map_err(|source| AssetError::ParseJson {
            what: "fetched npm-routes.json".to_owned(),
            source,
        })?;
    let client = build_jsdelivr_client(&index, &routes)?;
    Ok(ProbeSuccess {
        client,
        index_bytes,
        index,
    })
}

fn build_jsdelivr_client(index: &Index, routes: &NpmRoutes) -> Result<AssetClient, AssetError> {
    if routes.schema_version != NPM_ROUTES_SCHEMA {
        return Err(AssetError::UnsupportedNpmRoutesSchema {
            expected: NPM_ROUTES_SCHEMA,
            found: routes.schema_version,
        });
    }
    if routes.package_version != ASSET_VERSION {
        return Err(AssetError::NpmRoutesVersionMismatch {
            expected: ASSET_VERSION.to_owned(),
            found: routes.package_version.clone(),
        });
    }
    let mut package_by_asset_path = HashMap::with_capacity(index.devices.len());
    for (depot, entry) in &index.devices {
        let package = routes
            .devices
            .get(depot)
            .ok_or_else(|| AssetError::MissingNpmRoute {
                depot: depot.clone(),
            })?;
        package_by_asset_path.insert(
            entry.asset_path.trim_start_matches('/').to_owned(),
            package.clone(),
        );
    }
    Ok(AssetClient::new_jsdelivr(
        JSDELIVR_CATALOG_BASE,
        JSDELIVR_PACKAGE_ROOT,
        ASSET_VERSION,
        package_by_asset_path,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crate::{AssetError, DeviceEntry, Index};

    use super::{
        ASSET_VERSION, BuiltInSource, MirrorRace, NPM_ROUTES_SCHEMA, NpmRoutes, RaceOutcome, Step,
        build_jsdelivr_client,
    };

    const GRACE: Duration = Duration::from_millis(1_200);

    fn dummy_error() -> AssetError {
        AssetError::SourceProbeInterrupted
    }

    #[test]
    fn production_wins_even_when_a_pinned_mirror_answers_first() {
        let now = Instant::now();
        let mut race = MirrorRace::new();

        assert!(matches!(
            race.on_success(BuiltInSource::JsDelivr, 1, now, GRACE),
            Step::Continue { superseded: None }
        ));
        assert!(matches!(
            race.on_success(BuiltInSource::Production, 2, now, GRACE),
            Step::Done(RaceOutcome::Use(BuiltInSource::Production, 2))
        ));
    }

    #[test]
    fn pinned_mirror_wins_once_the_grace_window_expires() {
        let mut race: MirrorRace<u32> = MirrorRace::new();
        race.on_success(BuiltInSource::JsDelivr, 1, Instant::now(), GRACE);

        // The owning loop calls `conclude` once the receive wait times out
        // against `race.deadline()` — simulated directly, with no real sleep.
        assert!(matches!(
            race.conclude(),
            RaceOutcome::Use(BuiltInSource::JsDelivr, 1)
        ));
    }

    #[test]
    fn pinned_mirror_wins_immediately_once_production_has_already_failed() {
        let mut race = MirrorRace::new();

        assert!(matches!(
            race.on_failure(BuiltInSource::Production, dummy_error()),
            Step::Continue { superseded: None }
        ));
        assert!(matches!(
            race.on_success(BuiltInSource::Pages, 1, Instant::now(), GRACE),
            Step::Done(RaceOutcome::Use(BuiltInSource::Pages, 1))
        ));
    }

    #[test]
    fn a_second_pinned_success_is_reported_as_superseded_not_silently_dropped() {
        let now = Instant::now();
        let mut race: MirrorRace<u32> = MirrorRace::new();
        race.on_success(BuiltInSource::JsDelivr, 1, now, GRACE);

        // The first fallback stays the one `conclude` will use...
        assert!(matches!(
            race.on_success(BuiltInSource::Pages, 2, now, GRACE),
            Step::Continue {
                superseded: Some(BuiltInSource::Pages)
            }
        ));
        assert!(matches!(
            race.conclude(),
            RaceOutcome::Use(BuiltInSource::JsDelivr, 1)
        ));
    }

    #[test]
    fn every_source_failing_reports_all_three_errors() {
        let mut race: MirrorRace<u32> = MirrorRace::new();

        assert!(matches!(
            race.on_failure(BuiltInSource::Production, dummy_error()),
            Step::Continue { superseded: None }
        ));
        assert!(matches!(
            race.on_failure(BuiltInSource::Pages, dummy_error()),
            Step::Continue { superseded: None }
        ));
        assert!(matches!(
            race.on_failure(BuiltInSource::JsDelivr, dummy_error()),
            Step::Done(RaceOutcome::AllFailed { .. })
        ));
    }

    fn one_device_index() -> Index {
        Index {
            schema_version: 1,
            devices: HashMap::from([(
                "mx_master_3s".to_owned(),
                DeviceEntry {
                    model_id: "2b043".to_owned(),
                    model_ids: vec!["2b043".to_owned(), "2b034".to_owned()],
                    display_name: "MX Master 3S".to_owned(),
                    kind: "MOUSE".to_owned(),
                    asset_path: "v1/devices/mx_master_3s/".to_owned(),
                    files: Vec::new(),
                },
            )]),
        }
    }

    fn routes_for(package_version: &str) -> NpmRoutes {
        NpmRoutes {
            schema_version: NPM_ROUTES_SCHEMA,
            package_version: package_version.to_owned(),
            devices: HashMap::from([(
                "mx_master_3s".to_owned(),
                "@logi-assets/pointing".to_owned(),
            )]),
        }
    }

    #[test]
    fn npm_route_preserves_the_cloudflare_path_inside_its_shard() {
        let index = one_device_index();
        let routes = routes_for(ASSET_VERSION);

        let url = build_jsdelivr_client(&index, &routes)
            .ok()
            .and_then(|client| {
                client
                    .asset_url("v1/devices/mx_master_3s/", "front_core.png")
                    .ok()
            });

        assert_eq!(
            url.as_deref(),
            Some(
                "https://cdn.jsdelivr.net/npm/@logi-assets/pointing@0.1.0/v1/devices/mx_master_3s/front_core.png"
            )
        );
    }

    #[test]
    fn npm_routes_must_match_the_pinned_asset_version() {
        let index = one_device_index();
        let routes = routes_for("0.0.2");

        assert!(matches!(
            build_jsdelivr_client(&index, &routes),
            Err(AssetError::NpmRoutesVersionMismatch { .. })
        ));
    }

    #[test]
    fn every_catalog_depot_requires_an_npm_route() {
        let index = one_device_index();
        let routes = NpmRoutes {
            schema_version: NPM_ROUTES_SCHEMA,
            package_version: ASSET_VERSION.to_owned(),
            devices: HashMap::new(),
        };

        assert!(matches!(
            build_jsdelivr_client(&index, &routes),
            Err(AssetError::MissingNpmRoute { depot }) if depot == "mx_master_3s"
        ));
    }
}
