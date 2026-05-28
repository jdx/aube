use std::collections::{BTreeMap, BTreeSet};

use aube_lockfile::LockfileGraph;
use aube_resolver::MinimumReleaseAge;
use miette::{IntoDiagnostic, miette};
use tokio::task::JoinSet;

use super::settings;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgeViolation {
    name: String,
    version: String,
    published_at: String,
}

pub(super) async fn verify_frozen_lockfile_policy(
    cwd: &std::path::Path,
    graph: &LockfileGraph,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    network_mode: aube_registry::NetworkMode,
) -> miette::Result<()> {
    let Some(mra) = settings::resolve_minimum_release_age(settings_ctx, None) else {
        return Ok(());
    };
    let Some(cutoff) = mra.cutoff() else {
        return Ok(());
    };

    // `minimumReleaseAgeStrict=false` is a resolver fallback knob: when every
    // satisfying candidate is too new, fresh resolution may pick the lowest
    // satisfying version. Frozen installs do not have candidate ranges in hand
    // and cannot re-resolve safely, so the lockfile verification mirrors pnpm's
    // CI policy check and treats locked versions after the cutoff as rejected.
    let mut times = graph.times.clone();
    let missing = missing_time_entries(graph, &mra, &times);
    if !missing.is_empty() {
        let client =
            std::sync::Arc::new(crate::commands::make_client(cwd).with_network_mode(network_mode));
        let cache_dir = crate::commands::packument_full_cache_dir();
        let mut tasks = JoinSet::new();
        for (name, versions) in missing {
            let client = client.clone();
            let cache_dir = cache_dir.clone();
            tasks.spawn(async move {
                let packument = client
                    .fetch_packument_with_time_cached(&name, &cache_dir)
                    .await
                    .map_err(|err| match err {
                        aube_registry::Error::Offline(_) => {
                            missing_times_error(&name, versions.len())
                        }
                        other => miette::Report::new(other)
                            .wrap_err(format!("failed to fetch metadata for {name}")),
                    })?;
                Ok::<_, miette::Report>((name, versions, packument.time))
            });
        }
        while let Some(result) = tasks.join_next().await {
            let (_name, versions, fetched_times) = result.into_diagnostic()??;
            for key in versions {
                if let Some((_, version)) = key.rsplit_once('@')
                    && let Some(published_at) = fetched_times.get(version)
                {
                    times.insert(key, published_at.clone());
                }
            }
        }
    }

    let violations = minimum_release_age_violations(graph, &mra, &cutoff, &times);
    if violations.is_empty() {
        return Ok(());
    }

    let mut lines = Vec::with_capacity(violations.len().min(12) + 1);
    for v in violations.iter().take(12) {
        lines.push(format!(
            "  {}@{} was published at {}, within the minimumReleaseAge cutoff ({cutoff})",
            v.name, v.version, v.published_at
        ));
    }
    if violations.len() > lines.len() {
        lines.push(format!("  ... and {} more", violations.len() - lines.len()));
    }
    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_LOCKFILE_POLICY,
        help = "inspect recent lockfile changes before trusting them; if expected, regenerate the lockfile from a fresh resolution or relax minimumReleaseAge",
        "lockfile failed supply-chain policy check ({} entr{}):\n{}",
        violations.len(),
        if violations.len() == 1 { "y" } else { "ies" },
        lines.join("\n")
    ))
}

fn missing_times_error(name: &str, count: usize) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_LOCKFILE_POLICY,
        help = "run once with network access so aube can verify publish times, or set minimumReleaseAge=0 to disable this policy",
        "cannot verify minimumReleaseAge for {name}: lockfile is missing publish time{} for {} entr{} and network mode is offline",
        if count == 1 { "" } else { "s" },
        count,
        if count == 1 { "y" } else { "ies" },
    )
}

fn missing_time_entries(
    graph: &LockfileGraph,
    mra: &MinimumReleaseAge,
    times: &BTreeMap<String, String>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut missing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for pkg in graph.packages.values() {
        if pkg.local_source.is_some() || is_excluded(mra, pkg.name.as_str(), pkg.registry_name()) {
            continue;
        }
        let key = time_key(pkg.registry_name(), &pkg.version);
        if !times.contains_key(&key) {
            missing
                .entry(pkg.registry_name().to_string())
                .or_default()
                .insert(key);
        }
    }
    missing
}

fn minimum_release_age_violations(
    graph: &LockfileGraph,
    mra: &MinimumReleaseAge,
    cutoff: &str,
    times: &BTreeMap<String, String>,
) -> Vec<AgeViolation> {
    let mut seen = BTreeSet::new();
    let mut violations = Vec::new();
    for pkg in graph.packages.values() {
        if pkg.local_source.is_some() || is_excluded(mra, pkg.name.as_str(), pkg.registry_name()) {
            continue;
        }
        let key = time_key(pkg.registry_name(), &pkg.version);
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some(published_at) = times.get(&key) else {
            continue;
        };
        if published_at.as_str() > cutoff {
            violations.push(AgeViolation {
                name: pkg.registry_name().to_string(),
                version: pkg.version.clone(),
                published_at: published_at.clone(),
            });
        }
    }
    violations
}

fn time_key(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

fn is_excluded(mra: &MinimumReleaseAge, display_name: &str, registry_name: &str) -> bool {
    mra.exclude.contains(display_name) || mra.exclude.contains(registry_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::LockedPackage;

    fn graph_with(name: &str, version: &str, published_at: &str) -> LockfileGraph {
        let mut graph = LockfileGraph::default();
        let dep_path = format!("{name}@{version}");
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path: dep_path.clone(),
                ..LockedPackage::default()
            },
        );
        graph.times.insert(dep_path, published_at.to_string());
        graph
    }

    #[test]
    fn rejects_package_published_after_cutoff() {
        let graph = graph_with("demo", "1.0.0", "2026-05-28T08:12:56.230Z");
        let mra = MinimumReleaseAge {
            minutes: 1,
            exclude: BTreeSet::new().into_iter().collect(),
            strict: false,
        };
        let violations =
            minimum_release_age_violations(&graph, &mra, "2026-05-25T17:04:21.482Z", &graph.times);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].name, "demo");
    }

    #[test]
    fn exclude_skips_locked_package() {
        let graph = graph_with("demo", "1.0.0", "2026-05-28T08:12:56.230Z");
        let mra = MinimumReleaseAge {
            minutes: 1,
            exclude: ["demo".to_string()].into_iter().collect(),
            strict: false,
        };
        let violations =
            minimum_release_age_violations(&graph, &mra, "2026-05-25T17:04:21.482Z", &graph.times);
        assert!(violations.is_empty());
    }
}
