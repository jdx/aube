use crate::Error;

pub(super) fn encoded_name(name: &str) -> String {
    name.replace('/', "%2F")
}

/// `{registry}/-/package/{name}/dist-tags` — the ls endpoint.
pub(super) fn dist_tag_root_url(registry_url: &str, name: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
    )
}

/// `{registry}/-/package/{name}/dist-tags/{tag}` — the add/rm endpoint.
pub(super) fn dist_tag_url(registry_url: &str, name: &str, tag: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags/{}",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
        tag,
    )
}

/// Shared pre-flight mapping for dist-tag responses: turns 404 into
/// `NotFound(name)` and 401/403 into `Unauthorized`, so callers don't
/// have to repeat the same `if resp.status() == ...` ladder around
/// every PUT/GET. DELETE has a richer 404 shape (`name@tag`) and
/// inlines its own handling.
pub(super) fn check_dist_tag_status(resp: &reqwest::Response, name: &str) -> Result<(), Error> {
    match resp.status() {
        reqwest::StatusCode::NOT_FOUND => Err(Error::NotFound(name.to_string())),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            Err(Error::Unauthorized)
        }
        _ => Ok(()),
    }
}
