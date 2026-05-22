use super::body::{
    TIMEOUT_RETRY_CAP, check_body_cap, is_retriable_status, read_body_capped,
    read_body_capped_streaming_sha512, retry_after_from,
};
use super::cache::cached_is_fresh;
use super::{RegistryClient, encoded_name};
use crate::{Error, NetworkMode};

impl RegistryClient {
    pub(super) fn registry_url_for(&self, name: &str) -> &str {
        self.config.registry_for(name)
    }

    pub(super) fn force_cache(&self) -> bool {
        matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        )
    }

    pub(super) fn trust_cached_packument(
        &self,
        fetched_at: u64,
        max_age_secs: Option<u64>,
    ) -> bool {
        self.force_cache() || cached_is_fresh(fetched_at, max_age_secs)
    }

    /// Build `{registry}/{encoded_name}` — the packument route. Scoped
    /// packages have their `/` encoded as `%2F` so intermediate proxies
    /// that route on path segments (Artifactory's npm remote is the
    /// known offender) don't reject the request with 406. npm-cli and
    /// pnpm encode the same way.
    pub(super) fn packument_url(&self, name: &str) -> (String, &str) {
        let registry_url = self.registry_url_for(name);
        let url = format!(
            "{}/{}",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        (url, registry_url)
    }

    /// Build a GET request with auth headers for the given registry URL.
    pub(super) fn authed_get(&self, url: &str, registry_url: &str) -> reqwest::RequestBuilder {
        self.authed_request(reqwest::Method::GET, url, registry_url)
    }

    /// Build an HTTP request using this registry's configured TLS client
    /// and auth fallback order: bearer token, tokenHelper, then basic auth.
    pub fn authed_request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.authed(
            self.http_for(registry_url).request(method, url),
            registry_url,
        )
    }

    /// Build an HTTP request using the TLS/proxy client selected for this
    /// registry, but leave authentication to the caller. Publish uses this
    /// for npm Trusted Publishing exchange tokens so an old `.npmrc` token
    /// cannot be sent alongside the short-lived OIDC-derived bearer token.
    pub fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.http_for(registry_url).request(method, url)
    }

    pub fn has_resolved_auth_for(&self, registry_url: &str) -> bool {
        self.registry_auth_token_for(registry_url).is_some()
            || self.config.basic_auth_for(registry_url).is_some()
            || self.config.global_auth_token.is_some()
    }

    /// Cached equivalent of the previous free `same_host` function.
    /// The default registry never changes for the lifetime of the
    /// client, so the previous per-call `Url::parse(&self.config.registry)`
    /// was pure waste on every authed request. Comparison shape
    /// (scheme + host + port) is preserved byte-for-byte to keep the
    /// auth-leak guard semantics identical.
    fn same_host_as_default(&self, registry_url: &str) -> bool {
        let parsed_default = self
            .default_registry_parsed
            .get_or_init(|| reqwest::Url::parse(&self.config.registry).ok());
        let Some(a) = parsed_default.as_ref() else {
            return false;
        };
        let Ok(b) = reqwest::Url::parse(registry_url) else {
            return false;
        };
        a.scheme() == b.scheme()
            && a.host_str() == b.host_str()
            && a.port_or_known_default() == b.port_or_known_default()
    }

    /// Attach auth headers to any `RequestBuilder` keyed off the registry
    /// that owns `registry_url`. Shared between the GET helpers and the
    /// dist-tag / deprecate PUT calls so every write request picks up the
    /// same token/basic-auth resolution as reads. Future token-type
    /// changes (e.g. web-flow refresh) only have to be made here.
    pub(super) fn authed(
        &self,
        req: reqwest::RequestBuilder,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        if let Some(token) = self.registry_auth_token_for(registry_url) {
            req.bearer_auth(token)
        } else if let Some(auth) = self.config.basic_auth_for(registry_url) {
            req.header("Authorization", format!("Basic {auth}"))
        } else if let Some(token) = self.config.global_auth_token.as_ref()
            && self.same_host_as_default(registry_url)
        {
            // Only send the default _authToken when the request hits the
            // default registry. Stops a malicious scoped registry or a
            // packument with a dist.tarball pointing at attacker.example
            // from grabbing the user's npmjs token.
            req.bearer_auth(token)
        } else {
            req
        }
    }

    fn registry_auth_token_for(&self, registry_url: &str) -> Option<String> {
        // Fast path: memoized result. Hit on the second-and-later
        // request to the same registry URL within one process.
        if let Ok(cache) = self.auth_token_by_url.lock()
            && let Some(cached) = cache.get(registry_url)
        {
            return cached.clone();
        }
        let resolved = if let Some(auth) = self.config.registry_config_for(registry_url) {
            if let Some(token) = auth.auth_token.as_ref() {
                Some(token.to_string())
            } else if let Some(helper) = auth.token_helper.as_deref() {
                self.cached_token_helper_result(helper)
            } else {
                None
            }
        } else {
            None
        };
        if let Ok(mut cache) = self.auth_token_by_url.lock() {
            cache.insert(registry_url.to_string(), resolved.clone());
        }
        resolved
    }

    /// Cache key is the helper command itself, not the registry URL:
    /// `run_token_helper` spawns the helper as a subprocess that returns
    /// a token determined entirely by the command, with no URL input.
    /// Keying by URL would defeat the cache for tarball fetches (each
    /// tarball has a unique path) and re-spawn the helper hundreds of
    /// times during a large install.
    fn cached_token_helper_result(&self, helper: &str) -> Option<String> {
        {
            let cache = self.token_helper_cache.lock().ok()?;
            if let Some(token) = cache.get(helper) {
                return token.clone();
            }
        }
        let token = crate::config::run_token_helper(helper);
        if let Ok(mut cache) = self.token_helper_cache.lock() {
            cache.insert(helper.to_string(), token.clone());
        }
        token
    }

    pub(super) fn http_for(&self, registry_url: &str) -> &reqwest::Client {
        let uri_key = crate::config::registry_uri_key_pub(registry_url);
        crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key).unwrap_or(&self.http)
    }

    /// Pick the right HTTP client for tarball body downloads. The
    /// default registry uses the dedicated h1 client. Per-uri
    /// authed registries (corporate Artifactory, GitHub Packages)
    /// fall through to their h2 client because they're rare and
    /// keeping a parallel h1 map for them is not worth the
    /// complexity until measurement shows it matters.
    pub(super) fn http_tarball_for(&self, registry_url: &str) -> &reqwest::Client {
        let uri_key = crate::config::registry_uri_key_pub(registry_url);
        crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key)
            .unwrap_or(&self.http_tarball)
    }

    /// Authed RequestBuilder routed through the tarball-specific
    /// client. Mirrors [`Self::authed_get`] but picks
    /// [`Self::http_tarball_for`] instead of [`Self::http_for`].
    pub(super) fn authed_tarball_get(
        &self,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.authed(
            self.http_tarball_for(registry_url)
                .request(reqwest::Method::GET, url),
            registry_url,
        )
    }

    /// Same as [`Self::send_with_retry`] but also returns wall-clock
    /// elapsed from the first `.send()` to the returned response. Used
    /// by metadata call sites to compare against `fetchWarnTimeoutMs`
    /// without double-timing the retry backoff from caller code.
    pub(super) async fn send_with_retry_timed<F>(
        &self,
        build: F,
    ) -> Result<(reqwest::Response, std::time::Duration), reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let started = std::time::Instant::now();
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // Retry on 5xx server errors and 429 rate-limit.
                    // Everything else — 2xx/3xx successes and 4xx
                    // client errors the caller needs to see (404,
                    // 401, 403) — is returned verbatim.
                    if !is_retriable_status(status) || is_last {
                        return Ok((resp, started.elapsed()));
                    }
                    // 429 may carry a `Retry-After` header; honor it
                    // (seconds form) so a rate-limited registry gets
                    // the wait it asked for instead of our default
                    // exponential backoff. `make-fetch-happen` does
                    // the same. HTTP-date form is rare for npm and
                    // `chrono` isn't a dep — parse as u64 seconds or
                    // fall back to the computed backoff.
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    drop(resp);
                    // Surfaces at WARN so users see retry activity in
                    // the install output. The final failure still
                    // propagates up as a user-facing error if every
                    // attempt fails.
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = status.as_u16(),
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    if is_last {
                        return Err(e);
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %e,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        // `FetchPolicy::retries` is `u32`, so `max_attempts =
        // retries + 1` is always ≥ 1 and the loop runs at least once;
        // every path inside the loop either returns or continues. An
        // exit past this point is a structural bug, not a runtime
        // input the caller can provoke.
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Metadata-request wrapper around [`Self::send_with_retry_timed`]
    /// that records a slow-metadata entry when total wall-clock
    /// (including any retry backoff) exceeds `fetchWarnTimeoutMs`. `0`
    /// disables the recording, matching pnpm's convention and the
    /// default in `settings.toml`.
    ///
    /// Per-event detail goes into [`crate::slow_metadata`], not the
    /// log stream — the install pipeline emits one summary warning
    /// after resolve via [`crate::slow_metadata::flush_summary`].
    ///
    /// Not used by tarball downloads — `fetchMinSpeedKiBps` is the
    /// tarball-side observability knob, and the two warnings are
    /// semantically distinct (headers latency vs. body throughput).
    pub(super) async fn send_metadata_with_retry<F>(
        &self,
        label: &str,
        build: F,
    ) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let (resp, elapsed) = self.send_with_retry_timed(build).await?;
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = elapsed.as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            crate::slow_metadata::record(label, elapsed_ms, threshold);
        }
        Ok(resp)
    }

    pub(super) fn maybe_record_slow_metadata(&self, label: &str, started: std::time::Instant) {
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            crate::slow_metadata::record(label, elapsed_ms, threshold);
        }
    }

    /// Streaming variant of `retry_bytes_body_read`. Returns the body
    /// bytes along with a SHA-512 digest computed incrementally during
    /// the chunk read loop. Same retry semantics as the buffered path.
    /// Used by `fetch_tarball_bytes_streaming_sha512` so callers can
    /// skip the post-buffer hash pass.
    pub(super) async fn retry_bytes_body_read_streaming_sha512<F>(
        &self,
        label: &str,
        cap: u64,
        build: F,
    ) -> Result<(bytes::Bytes, [u8; 64], std::time::Duration), Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, cap, label)?;
                    let started = std::time::Instant::now();
                    match read_body_capped_streaming_sha512(resp, cap, label).await {
                        Ok((bytes, sha512)) => return Ok((bytes, sha512, started.elapsed())),
                        Err(err) if !is_last => {
                            let is_timeout = matches!(&err, Error::Http(e) if e.is_timeout());
                            if is_timeout && timeout_retries >= TIMEOUT_RETRY_CAP {
                                return Err(err);
                            }
                            if is_timeout {
                                timeout_retries += 1;
                            }
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_READ,
                                "retrying HTTP request after response body read error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    if err.is_timeout() {
                        if timeout_retries >= TIMEOUT_RETRY_CAP {
                            return Err(Error::Http(err));
                        }
                        timeout_retries += 1;
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(Error::Http(err)),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    pub(super) async fn retry_bytes_body_read<F>(
        &self,
        label: &str,
        cap: u64,
        build: F,
    ) -> Result<(bytes::Bytes, std::time::Duration), Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSIENT,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, cap, label)?;
                    let started = std::time::Instant::now();
                    match read_body_capped(resp, cap, label).await {
                        Ok(bytes) => return Ok((bytes, started.elapsed())),
                        Err(err) if !is_last => {
                            let is_timeout = matches!(&err, Error::Http(e) if e.is_timeout());
                            if is_timeout && timeout_retries >= TIMEOUT_RETRY_CAP {
                                return Err(err);
                            }
                            if is_timeout {
                                timeout_retries += 1;
                            }
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_BODY_READ,
                                "retrying HTTP request after response body read error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    if err.is_timeout() {
                        if timeout_retries >= TIMEOUT_RETRY_CAP {
                            return Err(Error::Http(err));
                        }
                        timeout_retries += 1;
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        code = aube_codes::warnings::WARN_AUBE_HTTP_RETRY_TRANSPORT,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(Error::Http(err)),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }
}
