use crate::Error;
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use aube_util::adaptive::AdaptiveLimit;
use std::path::PathBuf;
use std::sync::Arc;

/// Inputs the packument-fetch task needs once it's spawned.
///
/// All fields are owned/`Arc`-cloned so the future can be moved into
/// the resolver's `JoinSet` without borrowing the outer scope.
pub(super) struct FetchInputs {
    pub(super) name: String,
    pub(super) client: Arc<RegistryClient>,
    pub(super) cache_dir: Option<PathBuf>,
    pub(super) full_cache_dir: Option<PathBuf>,
    /// Precomputed from the resolver's `minimum_release_age` exclude
    /// list and `published_by` cutoff — if false, the primer is
    /// bypassed even when it would otherwise be eligible.
    pub(super) primer_covers_cutoff: bool,
    /// `force_metadata_primer` from the resolver: when true, use the
    /// primer even for non-default registries (and rewrite tarball URLs
    /// to the active registry).
    pub(super) force_metadata_primer: bool,
    pub(super) sem: Arc<AdaptiveLimit>,
    /// True when the caller needs the packument's `time:` map and
    /// must therefore use the full-packument path.
    pub(super) needs_time: bool,
}

/// Body of the per-packument fetch task spawned by the resolver.
///
/// Returns `(name, packument, from_primer)` — `from_primer` is true
/// when the result came from the bundled metadata primer (only its
/// capped slice of high-traffic histories), so the caller knows a
/// range miss must trigger a live registry refetch before reporting
/// `ERR_AUBE_NO_MATCHING_VERSION`.
pub(super) async fn fetch_one_packument(
    inputs: FetchInputs,
) -> Result<(String, Packument, bool), Error> {
    let FetchInputs {
        name,
        client,
        cache_dir,
        full_cache_dir,
        primer_covers_cutoff,
        force_metadata_primer,
        sem,
        needs_time,
    } = inputs;
    let _diag_span =
        aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "packument_fetch")
            .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)));
    let _diag_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Pack);
    let permit_wait = std::time::Instant::now();
    let permit = sem.acquire().await;
    let permit_wait_ms = permit_wait.elapsed();
    if permit_wait_ms.as_millis() > 1 {
        aube_util::diag::event_lazy(
            aube_util::diag::Category::Resolver,
            "packument_permit_wait",
            permit_wait_ms,
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
    }
    aube_util::diag::attribute_wait(aube_util::diag::Slot::Pack, &name, permit_wait_ms);
    let _holder_guard = aube_util::diag::register_holder(aube_util::diag::Slot::Pack, &name);
    let mut cached = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => client.cached_full_packument_lookup(&name, dir),
            None => Default::default(),
        }
    } else if let Some(ref dir) = cache_dir {
        client.cached_packument_lookup(&name, dir)
    } else {
        Default::default()
    };
    if let Some(packument) = cached.packument.take() {
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_disk_hit",
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
        permit.record_cancelled();
        return Ok((name, packument, false));
    }
    let use_metadata_primer = (force_metadata_primer
        || client.uses_default_npm_registry_for(&name))
        && primer_covers_cutoff;
    if use_metadata_primer
        && !cached.stale
        && let Some(seed) = crate::primer::get(&name)
    {
        let mut packument = seed.packument();
        if force_metadata_primer {
            for version in packument.versions.values_mut() {
                let tarball = client.tarball_url(&version.name, &version.version);
                version.dist = version.dist.take().map(|mut dist| {
                    dist.tarball = tarball;
                    dist
                });
            }
        }
        if needs_time {
            if let Some(dir) = full_cache_dir.as_ref() {
                client.seed_full_packument_cache(
                    &name,
                    dir,
                    &packument,
                    seed.etag.as_deref(),
                    seed.last_modified.as_deref(),
                    false,
                );
            }
        } else if let Some(dir) = cache_dir.as_ref() {
            client.seed_packument_cache(
                &name,
                dir,
                &packument,
                seed.etag.as_deref(),
                seed.last_modified.as_deref(),
                false,
            );
        }
        aube_util::diag::instant_lazy(
            aube_util::diag::Category::Resolver,
            "packument_primer_hit",
            || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
        );
        permit.record_cancelled();
        return Ok((name, packument, true));
    }
    let fetch_outcome = if needs_time {
        match full_cache_dir.as_ref() {
            Some(dir) => {
                client
                    .fetch_packument_with_time_cached_after_lookup(&name, dir, cached)
                    .await
            }
            None => client.fetch_packument(&name).await,
        }
    } else if let Some(ref dir) = cache_dir {
        client
            .fetch_packument_cached_after_lookup(&name, dir, cached)
            .await
    } else {
        client.fetch_packument(&name).await
    };
    let packument = match fetch_outcome {
        Ok(p) => {
            permit.record_success();
            p
        }
        Err(e) => {
            if e.is_throttle() {
                permit.record_throttle();
            } else {
                permit.record_cancelled();
            }
            return Err(Error::Registry(name.clone(), e.to_string()));
        }
    };
    aube_util::diag::instant_lazy(
        aube_util::diag::Category::Resolver,
        "packument_network_hit",
        || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&name)),
    );
    Ok((name, packument, false))
}
