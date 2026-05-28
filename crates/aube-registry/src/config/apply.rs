use base64::Engine as _;
use std::collections::BTreeSet;
use std::path::PathBuf;

use super::env::env_any;
use super::token::sanitize_token_helper;
use super::types::{AuthConfig, NpmConfig, NpmrcSource};
use super::url::{
    is_public_npmjs_url, lookup_by_uri_prefix, normalize_npmrc_uri_key, normalize_registry_url,
    package_scope, registry_uri_key,
};
use super::util::{non_empty, pem_value};

impl NpmConfig {
    /// Register default scope→registry mappings that aube ships with
    /// out of the box. Currently only `@jsr` → <https://npm.jsr.io/>,
    /// which lets `jsr:` specs work without the user touching `.npmrc`.
    /// User-provided `.npmrc` entries win — `apply` has already run by
    /// the time we get here, so we only fill in gaps.
    pub(super) fn apply_builtin_scoped_defaults(&mut self) {
        self.scoped_registries
            .entry(crate::jsr::JSR_NPM_SCOPE.to_string())
            .or_insert_with(|| crate::jsr::JSR_DEFAULT_REGISTRY.to_string());
    }

    /// Fallback-only: populate proxy/no_proxy from the standard
    /// `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` environment variables
    /// when the `.npmrc` layer didn't already set them. A value from
    /// `.npmrc` wins over env so project configuration stays explicit.
    /// Resolve proxy/no_proxy fields using the same precedence
    /// chain pnpm's config reader applies (see
    /// `config/reader/src/index.ts` lines 559-568 in the pnpm
    /// repo):
    ///
    /// - `httpsProxy` ← `.npmrc httpsProxy` ?? `.npmrc proxy` ??
    ///   env `HTTPS_PROXY`/`https_proxy`
    /// - `httpProxy` ← `.npmrc httpProxy` ?? resolved `httpsProxy`
    ///   ?? env `HTTP_PROXY`/`http_proxy` ?? env `PROXY`/`proxy`
    /// - `noProxy` ← `.npmrc noProxy` ?? env `NO_PROXY`/`no_proxy`
    ///
    /// Note that `httpsProxy` does **not** fall back to
    /// `HTTP_PROXY`: pnpm (and npm) only inherit the HTTP proxy
    /// downward into HTTPS, never upward. The `httpProxy` field
    /// *does* inherit whatever `httpsProxy` resolved to, so a
    /// single `https-proxy=...` line in `.npmrc` configures both.
    pub fn apply_proxy_env(&mut self) {
        if self.https_proxy.is_none() {
            self.https_proxy = self
                .npmrc_proxy
                .clone()
                .or_else(|| env_any(&["HTTPS_PROXY", "https_proxy"]));
        }
        if self.http_proxy.is_none() {
            self.http_proxy = self
                .https_proxy
                .clone()
                .or_else(|| env_any(&["HTTP_PROXY", "http_proxy"]))
                .or_else(|| env_any(&["PROXY", "proxy"]));
        }
        if self.no_proxy.is_none() {
            self.no_proxy = env_any(&["NO_PROXY", "no_proxy"]);
        }
    }

    /// Get the registry URL for a given package name.
    pub fn registry_for(&self, package_name: &str) -> &str {
        if let Some(scope) = package_scope(package_name)
            && let Some(url) = self.scoped_registries.get(&scope.to_lowercase())
        {
            return url;
        }
        &self.registry
    }

    /// True when `package_name` resolves through the public
    /// `registry.npmjs.org` registry. Used by supply-chain gates
    /// (`crates/aube/src/commands/add_supply_chain.rs`) to skip
    /// public-only signals (OSV `MAL-*` advisories, npmjs weekly
    /// downloads) on packages a private/internal registry is the
    /// source of truth for. The default registry being swapped out
    /// (`registry=https://internal.example/`) or a scoped override
    /// (`@myorg:registry=https://...`) both cause this to return
    /// `false` so internal packages don't trip the gates.
    pub fn is_public_npmjs(&self, package_name: &str) -> bool {
        is_public_npmjs_url(self.registry_for(package_name))
    }

    /// Get the auth token for a given registry URL.
    pub fn auth_token_for(&self, registry_url: &str) -> Option<&str> {
        self.registry_config_for(registry_url)
            .and_then(|auth| auth.auth_token.as_deref())
    }

    pub fn token_helper_for(&self, registry_url: &str) -> Option<&str> {
        self.registry_config_for(registry_url)
            .and_then(|auth| auth.token_helper.as_deref())
    }

    /// Get the basic auth (_auth) for a given registry URL.
    pub fn basic_auth_for(&self, registry_url: &str) -> Option<String> {
        let auth = self.registry_config_for(registry_url)?;
        if let Some(ref a) = auth.auth {
            return Some(a.clone());
        }
        let username = auth.username.as_ref()?;
        let password = auth.password.as_ref()?;
        let password = base64::engine::general_purpose::STANDARD
            .decode(password)
            .ok()?;
        let mut raw = Vec::with_capacity(username.len() + 1 + password.len());
        raw.extend_from_slice(username.as_bytes());
        raw.push(b':');
        raw.extend_from_slice(&password);
        Some(base64::engine::general_purpose::STANDARD.encode(raw))
    }

    pub fn registry_config_for(&self, registry_url: &str) -> Option<&AuthConfig> {
        let uri_key = registry_uri_key(registry_url);
        lookup_by_uri_prefix(&self.auth_by_uri, &uri_key)
    }

    /// Test-only compatibility shim. Production code must go through
    /// `apply_tagged` with real source tags so the subprocess-settings
    /// gate fires correctly. Tests that legitimately emulate a
    /// user-scope-only environment can use this helper to avoid
    /// rewriting every fixture.
    #[cfg(test)]
    pub(super) fn apply(&mut self, entries: Vec<(String, String)>) {
        self.apply_tagged(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::User, k, v))
                .collect(),
        );
    }

    pub(super) fn apply_tagged(&mut self, entries: Vec<(NpmrcSource, String, String)>) {
        let mut user_registry = "https://registry.npmjs.org/".to_string();
        let mut pnpm_auth_registry = "https://registry.npmjs.org/".to_string();
        let mut project_registry = "https://registry.npmjs.org/".to_string();
        let mut npmrc_auth_file_registry = "https://registry.npmjs.org/".to_string();
        let mut env_registry = "https://registry.npmjs.org/".to_string();
        let mut explicit_uri_fields = BTreeSet::new();

        for (source, key, value) in &entries {
            if key == "registry" {
                *source_registry_mut(
                    *source,
                    &mut user_registry,
                    &mut pnpm_auth_registry,
                    &mut project_registry,
                    &mut npmrc_auth_file_registry,
                    &mut env_registry,
                ) = normalize_registry_url(value);
            } else if key.starts_with("//")
                && let Some((uri, suffix)) = key.rsplit_once(':')
                && let Some(suffix) = canonical_rescoped_suffix(suffix)
            {
                explicit_uri_fields.insert((normalize_npmrc_uri_key(uri), suffix));
            }
        }

        for (source, key, value) in entries {
            if key == "registry" {
                self.registry = normalize_registry_url(&value);
            } else if key == "_authToken" {
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_authToken",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_authToken").unwrap_or("_authToken"),
                    )),
                    |auth| auth.auth_token = Some(value),
                );
            } else if key == "_auth" {
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_auth",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_auth").unwrap_or("_auth"),
                    )),
                    |auth| auth.auth = Some(value),
                );
            } else if key == "username" {
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "username",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("username").unwrap_or("username"),
                    )),
                    |auth| auth.username = Some(value),
                );
            } else if key == "_password" {
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_password",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_password").unwrap_or("_password"),
                    )),
                    |auth| auth.password = Some(value),
                );
            } else if matches!(key.as_str(), "cert" | "key") {
                let suffix = key.clone();
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                let explicit_uri_field = explicit_uri_fields.contains(&(
                    registry_uri_key(registry),
                    canonical_rescoped_suffix(&suffix).unwrap_or(suffix.as_str()),
                ));
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    &suffix,
                    explicit_uri_field,
                    |auth| {
                        if suffix == "cert" {
                            auth.tls.cert = Some(pem_value(value));
                        } else {
                            auth.tls.key = Some(pem_value(value));
                        }
                    },
                );
            } else if matches!(key.as_str(), "tokenHelper" | "token-helper") {
                if !source.is_trusted_for_subprocess_settings() {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_TOKEN_HELPER,
                        "ignoring tokenHelper from untrusted source {source:?}: committed `.npmrc` cannot set this"
                    );
                    continue;
                }
                let Some(sanitized) = sanitize_token_helper(&value) else {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_TOKEN_HELPER,
                        "ignoring tokenHelper: value is not a bare absolute path: {value:?}"
                    );
                    continue;
                };
                let registry = source_registry(
                    source,
                    &user_registry,
                    &pnpm_auth_registry,
                    &project_registry,
                    &npmrc_auth_file_registry,
                    &env_registry,
                );
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "tokenHelper",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("tokenHelper").unwrap_or("tokenHelper"),
                    )),
                    |auth| auth.token_helper = Some(sanitized),
                );
            } else if matches!(
                key.as_str(),
                "https-proxy"
                    | "httpsProxy"
                    | "http-proxy"
                    | "httpProxy"
                    | "proxy"
                    | "noproxy"
                    | "noProxy"
                    | "no-proxy"
            ) {
                // Proxies redirect every registry request through a
                // third party for the rest of the process. A
                // project-committed `.npmrc` must not be able to set
                // that for everyone who clones the repository, same
                // trust gate `strict-ssl` and `tokenHelper` already
                // apply.
                if !source.is_trusted_for_subprocess_settings() {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_PROXY,
                        "ignoring {key} from untrusted source {source:?}: committed `.npmrc` cannot set registry proxies"
                    );
                } else {
                    match key.as_str() {
                        "https-proxy" | "httpsProxy" => {
                            self.https_proxy = non_empty(value);
                        }
                        "http-proxy" | "httpProxy" => {
                            self.http_proxy = non_empty(value);
                        }
                        "proxy" => {
                            // pnpm treats `.npmrc proxy=` as the
                            // fallback source for `httpsProxy` (and,
                            // transitively, `httpProxy`) — not as a
                            // direct alias for `httpProxy`. See the
                            // `apply_proxy_env` resolution chain.
                            self.npmrc_proxy = non_empty(value);
                        }
                        _ => {
                            self.no_proxy = non_empty(value);
                        }
                    }
                }
            } else if matches!(key.as_str(), "strict-ssl" | "strictSsl") {
                if let Some(b) = aube_settings::parse_bool(&value) {
                    // strict-ssl=false kills TLS cert validation for
                    // the whole client. A project-committed .npmrc
                    // must never flip this for the whole install. Only
                    // user or global scope can disable validation.
                    // Same trust gate tokenHelper already uses.
                    if !b && !source.is_trusted_for_subprocess_settings() {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_STRICT_SSL_DISABLE,
                            "ignoring strict-ssl=false: {source:?} source is not trusted (committed `.npmrc` cannot disable TLS validation)"
                        );
                    } else {
                        self.strict_ssl = b;
                    }
                }
            } else if matches!(key.as_str(), "local-address" | "localAddress") {
                match value.trim().parse::<std::net::IpAddr>() {
                    Ok(ip) => self.local_address = Some(ip),
                    Err(e) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_LOCAL_ADDRESS,
                        "ignoring invalid local-address {value:?}: {e}"
                    ),
                }
            } else if key == "maxsockets" {
                match value.trim().parse::<usize>() {
                    Ok(n) if n > 0 => self.max_sockets = Some(n),
                    Ok(_) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_MAXSOCKETS,
                        "ignoring maxsockets=0"
                    ),
                    Err(e) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_MAXSOCKETS,
                        "ignoring invalid maxsockets {value:?}: {e}"
                    ),
                }
            } else if matches!(key.as_str(), "cafile" | "caFile") {
                // Top-level (unscoped) cafile — applies to all registries.
                // Diverges from the URI-scoped form in the `//` block
                // below; both can coexist and stack additively.
                self.cafile = Some(PathBuf::from(value));
            } else if matches!(key.as_str(), "ca" | "ca[]") {
                // Top-level inline PEM, single or array form. npm/pnpm
                // accept repeated `ca[]=...` lines to build up a list;
                // mirror that by pushing instead of replacing.
                self.ca.push(pem_value(value));
            } else if let Some(scope) = key.strip_suffix(":registry") {
                if scope.starts_with('@') {
                    self.scoped_registries
                        .insert(scope.to_lowercase(), normalize_registry_url(&value));
                }
            } else if key.starts_with("//") {
                // URI-specific config: //registry.url/:_authToken=TOKEN
                if let Some((uri, suffix)) = key.rsplit_once(':') {
                    // Normalize so `//host:443/x/` and `//host/x/` collapse
                    // to the same key — matches what `registry_uri_key`
                    // produces on the lookup side after stripping the
                    // scheme's default port.
                    let entry = self
                        .auth_by_uri
                        .entry(normalize_npmrc_uri_key(uri))
                        .or_default();
                    match suffix {
                        "_authToken" => entry.auth_token = Some(value),
                        "_auth" => entry.auth = Some(value),
                        "username" => entry.username = Some(value),
                        "_password" => entry.password = Some(value),
                        "tokenHelper" | "token-helper" => {
                            // CVE-2025-69262 (pnpm GHSA-2phv-j68v-wwqx)
                            // class: `tokenHelper` is spawned as
                            // `sh -c <value>` on unix or `cmd /C
                            // <value>` on Windows at the next authed
                            // registry request. Accept only from
                            // trusted sources and only when the
                            // value parses as a sanitized absolute
                            // path to an interpreter.
                            if !source.is_trusted_for_subprocess_settings() {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_TOKEN_HELPER,
                                    "ignoring tokenHelper for {uri}: {source:?} source is not trusted for subprocess settings (committed `.npmrc` cannot set this)"
                                );
                                continue;
                            }
                            let Some(sanitized) = sanitize_token_helper(&value) else {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_INVALID_TOKEN_HELPER,
                                    "ignoring tokenHelper for {uri}: value is not a bare absolute path: {value:?}"
                                );
                                continue;
                            };
                            entry.token_helper = Some(sanitized);
                        }
                        "ca" | "ca[]" => entry.tls.ca.push(pem_value(value)),
                        "cafile" | "caFile" => entry.tls.cafile = Some(PathBuf::from(value)),
                        "cert" => entry.tls.cert = Some(pem_value(value)),
                        "key" => entry.tls.key = Some(pem_value(value)),
                        _ => {} // Ignore unknown suffixes for now
                    }
                }
            }
            // Generic pnpm settings (`auto-install-peers`, etc) are NOT
            // matched here — they're resolved by aube's settings
            // module against the raw entries, using the canonical
            // source list from settings.toml. Add a new branch here
            // only if the key maps to a registry-client concept.
        }
    }

    fn rescope_unscoped_registry_setting(
        &mut self,
        source: NpmrcSource,
        registry: &str,
        suffix: &str,
        explicit_uri_field_exists: bool,
        apply: impl FnOnce(&mut AuthConfig),
    ) {
        if matches!(source, NpmrcSource::Env | NpmrcSource::PnpmAuth) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSCOPED_AUTH_RESCOPED,
                "unscoped {suffix} from {source:?} was pinned to {registry}"
            );
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSCOPED_AUTH_RESCOPED,
                "unscoped {suffix} from {source:?} was pinned to {registry}; write `{}:{suffix}=...` instead",
                registry_uri_key(registry)
            );
        }
        let entry = self
            .auth_by_uri
            .entry(registry_uri_key(registry))
            .or_default();
        if !explicit_uri_field_exists {
            apply(entry);
        }
    }
}

fn canonical_rescoped_suffix(suffix: &str) -> Option<&'static str> {
    match suffix {
        "_authToken" => Some("_authToken"),
        "_auth" => Some("_auth"),
        "username" => Some("username"),
        "_password" => Some("_password"),
        "cert" => Some("cert"),
        "key" => Some("key"),
        "tokenHelper" | "token-helper" => Some("tokenHelper"),
        _ => None,
    }
}

fn source_registry<'a>(
    source: NpmrcSource,
    user: &'a str,
    pnpm_auth: &'a str,
    project: &'a str,
    npmrc_auth_file: &'a str,
    env: &'a str,
) -> &'a str {
    match source {
        NpmrcSource::User => user,
        NpmrcSource::PnpmAuth => pnpm_auth,
        NpmrcSource::Project => project,
        NpmrcSource::NpmrcAuthFile => npmrc_auth_file,
        NpmrcSource::Env => env,
    }
}

fn source_registry_mut<'a>(
    source: NpmrcSource,
    user: &'a mut String,
    pnpm_auth: &'a mut String,
    project: &'a mut String,
    npmrc_auth_file: &'a mut String,
    env: &'a mut String,
) -> &'a mut String {
    match source {
        NpmrcSource::User => user,
        NpmrcSource::PnpmAuth => pnpm_auth,
        NpmrcSource::Project => project,
        NpmrcSource::NpmrcAuthFile => npmrc_auth_file,
        NpmrcSource::Env => env,
    }
}
