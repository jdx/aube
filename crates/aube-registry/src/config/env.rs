/// Synthesize `.npmrc`-style entries from a captured `npm_config_*` /
/// `NPM_CONFIG_*` environment-variable slice so [`NpmConfig::apply`]
/// can consume them uniformly. Only registry-client-owned keys (the
/// default registry, scoped registries, per-URI auth, proxies, TLS
/// knobs) are emitted — generic pnpm settings are already surfaced
/// via `aube_settings::resolved::*`, which consults its own env-var
/// aliases. Env entries must be applied *after* `.npmrc` entries so
/// last-write-wins gives env the higher precedence npm/pnpm document.
pub(super) fn npm_config_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut npm_scoped = Vec::new();
    let mut pnpm_scoped = Vec::new();
    let mut out = Vec::new();
    for (name, value) in env {
        if value.is_empty() {
            continue;
        }
        match translate_npm_config_env(name, value) {
            Some((key, value)) if key.starts_with("//") => {
                if name
                    .get(.."pnpm_config_".len())
                    .is_some_and(|p| p.eq_ignore_ascii_case("pnpm_config_"))
                {
                    pnpm_scoped.push((key, value));
                } else {
                    npm_scoped.push((key, value));
                }
            }
            Some(entry) => out.push(entry),
            None => {}
        }
    }
    out.extend(npm_scoped);
    out.extend(pnpm_scoped);
    out
}

/// Map a single `npm_config_*` / `NPM_CONFIG_*` env var to the
/// `.npmrc`-style `(key, value)` that [`NpmConfig::apply`] understands.
/// Returns `None` for env vars unrelated to registry-client config —
/// those are owned by the generic settings resolver. Pure function so
/// tests can exercise the mapping without mutating `std::env`.
pub(super) fn translate_npm_config_env(name: &str, value: &str) -> Option<(String, String)> {
    let suffix = name
        .strip_prefix("npm_config_")
        .or_else(|| name.strip_prefix("NPM_CONFIG_"))
        .or_else(|| strip_url_scoped_config_prefix(name))?;
    // Per-URI auth keys (e.g. `//registry.example.com/:_authToken`)
    // already carry `.npmrc` syntax in the env-var name. Pass them
    // through unchanged so `apply`'s `starts_with("//")` arm picks
    // them up and preserves the `_authToken` / `_auth` / `username`
    // casing that the match inside it depends on.
    if suffix.starts_with("//") && is_url_scoped_env_auth_key(suffix) {
        return Some((suffix.to_string(), value.to_string()));
    }
    // Scoped-registry keys: `@myorg:REGISTRY` or `@MYORG:registry`,
    // translated to the canonical `@myorg:registry` form. The scope
    // segment is lowercased because npm scope names are
    // case-insensitive on the registry side, and `apply` matches the
    // `:registry` suffix literally.
    if let Some(rest) = suffix.strip_prefix('@')
        && let Some((scope, tail)) = rest.split_once(':')
        && tail.eq_ignore_ascii_case("registry")
    {
        return Some((
            format!("@{}:registry", scope.to_ascii_lowercase()),
            value.to_string(),
        ));
    }
    // Canonical single-word or `_`-separated multi-word keys. The
    // left column is the lowercased env-suffix (POSIX-style); the
    // right column is the `.npmrc` key `apply` matches on.
    let npmrc_key = match suffix.to_ascii_lowercase().as_str() {
        "registry" => "registry",
        "https_proxy" => "https-proxy",
        "http_proxy" => "http-proxy",
        "proxy" => "proxy",
        "noproxy" => "noproxy",
        "strict_ssl" => "strict-ssl",
        "local_address" => "local-address",
        "maxsockets" => "maxsockets",
        _ => return None,
    };
    Some((npmrc_key.to_string(), value.to_string()))
}

fn strip_url_scoped_config_prefix(name: &str) -> Option<&str> {
    for prefix in ["npm_config_", "pnpm_config_"] {
        if name
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        {
            let suffix = &name[prefix.len()..];
            if suffix.starts_with("//") {
                return Some(suffix);
            }
        }
    }
    None
}

fn is_url_scoped_env_auth_key(key: &str) -> bool {
    key.rsplit_once(':').is_some_and(|(_, suffix)| {
        matches!(suffix, "_authToken" | "_auth" | "username" | "_password")
    })
}
/// Return the first set (and non-empty) env var in `names`. Used to
/// read proxy config from both the upper- and lowercase spellings that
/// curl / node conventionally accept.
pub(super) fn env_any(names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                // Trim before returning so a shell-quoted value like
                // `HTTPS_PROXY=" http://proxy "` doesn't slip past
                // `reqwest::Proxy::https` with surrounding whitespace
                // and silently fail.
                return Some(trimmed.to_string());
            }
        }
    }
    None
}
