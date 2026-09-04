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

/// Result of [`race_sources`]: which source answered, or why none did.
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

/// Arbitrate the three probe threads' results.
///
/// Production wins immediately. A pinned mirror (Pages or jsDelivr)
/// answering first only wins once either Production has also failed, or
/// `grace` has elapsed without Production answering — see
/// [`PRODUCTION_GRACE`] for why an unconditional speed race is wrong here.
fn race_sources<T>(
    receiver: &mpsc::Receiver<(BuiltInSource, Result<T, AssetError>)>,
    grace: Duration,
) -> RaceOutcome<T> {
    let mut production_error = None;
    let mut pages_error = None;
    let mut jsdelivr_error = None;
    let mut pinned_fallback: Option<(BuiltInSource, T)> = None;
    let mut deadline: Option<Instant> = None;

    loop {
        let event = match deadline {
            Some(deadline) => {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let (source, result) = match event {
            Ok(pair) => pair,
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                if pinned_fallback.is_some() {
                    break;
                }
                return RaceOutcome::Interrupted;
            }
        };

        match result {
            Ok(probe) => {
                if matches!(source, BuiltInSource::Production) {
                    return RaceOutcome::Use(source, probe);
                }
                if production_error.is_some() {
                    // Production is already known dead — nothing left to wait for.
                    return RaceOutcome::Use(source, probe);
                }
                if pinned_fallback.is_none() {
                    pinned_fallback = Some((source, probe));
                    deadline = Some(Instant::now() + grace);
                }
            }
            Err(error) => {
                warn!(%source, error = ?error, "asset source probe failed");
                match source {
                    BuiltInSource::Production => {
                        production_error = Some(error);
                        if pinned_fallback.is_some() {
                            break;
                        }
                    }
                    BuiltInSource::Pages => pages_error = Some(error),
                    BuiltInSource::JsDelivr => jsdelivr_error = Some(error),
                }
                if production_error.is_some() && pages_error.is_some() && jsdelivr_error.is_some() {
                    break;
                }
            }
        }
    }

    if let Some((source, probe)) = pinned_fallback {
        return RaceOutcome::Use(source, probe);
    }
    match (production_error, pages_error, jsdelivr_error) {
        (Some(production), Some(pages), Some(jsdelivr)) => RaceOutcome::AllFailed {
            production: Box::new(production),
            pages: Box::new(pages),
            jsdelivr: Box::new(jsdelivr),
        },
        _ => RaceOutcome::Interrupted,
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
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use crate::{AssetError, DeviceEntry, Index};

    use super::{
        ASSET_VERSION, BuiltInSource, NPM_ROUTES_SCHEMA, NpmRoutes, RaceOutcome,
        build_jsdelivr_client, race_sources,
    };

    const TEST_GRACE: Duration = Duration::from_millis(60);

    fn dummy_error() -> AssetError {
        AssetError::SourceProbeInterrupted
    }

    /// Sends `events` on a fresh channel, each after its given delay, then
    /// hands `race_sources` the receiving end with `TEST_GRACE`.
    fn race(events: Vec<(Duration, BuiltInSource, Result<u32, AssetError>)>) -> RaceOutcome<u32> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (delay, source, result) in events {
                thread::sleep(delay);
                let _ignore_disconnect = sender.send((source, result));
            }
        });
        race_sources(&receiver, TEST_GRACE)
    }

    #[test]
    fn production_wins_even_when_a_pinned_mirror_answers_first() {
        let outcome = race(vec![
            (Duration::ZERO, BuiltInSource::JsDelivr, Ok(1)),
            (Duration::from_millis(20), BuiltInSource::Production, Ok(2)),
        ]);

        assert!(matches!(
            outcome,
            RaceOutcome::Use(BuiltInSource::Production, 2)
        ));
    }

    #[test]
    fn pinned_mirror_wins_once_the_grace_window_expires() {
        let outcome = race(vec![(Duration::ZERO, BuiltInSource::JsDelivr, Ok(1))]);

        assert!(matches!(
            outcome,
            RaceOutcome::Use(BuiltInSource::JsDelivr, 1)
        ));
    }

    #[test]
    fn pinned_mirror_wins_immediately_once_production_has_already_failed() {
        let outcome = race(vec![
            (
                Duration::ZERO,
                BuiltInSource::Production,
                Err(dummy_error()),
            ),
            (Duration::from_millis(5), BuiltInSource::Pages, Ok(1)),
        ]);

        assert!(matches!(outcome, RaceOutcome::Use(BuiltInSource::Pages, 1)));
    }

    #[test]
    fn every_source_failing_reports_all_three_errors() {
        let outcome = race(vec![
            (
                Duration::ZERO,
                BuiltInSource::Production,
                Err(dummy_error()),
            ),
            (Duration::ZERO, BuiltInSource::Pages, Err(dummy_error())),
            (Duration::ZERO, BuiltInSource::JsDelivr, Err(dummy_error())),
        ]);

        assert!(matches!(outcome, RaceOutcome::AllFailed { .. }));
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
