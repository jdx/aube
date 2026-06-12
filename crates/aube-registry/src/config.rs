mod apply;
mod env;
mod fetch;
mod load;
mod npmrc;
mod token;
mod types;
mod url;
mod util;

#[cfg(test)]
mod tests;

pub use fetch::FetchPolicy;
pub use load::{SplitNpmrcEntries, load_npmrc_entries, load_npmrc_entries_split};
pub use types::{AuthConfig, NpmConfig, TlsConfig};
pub use url::{normalize_registry_url_pub, registry_uri_key_pub};

pub(crate) use token::run_token_helper;
pub(crate) use url::lookup_by_uri_prefix;

#[cfg(test)]
use env::{npm_config_env_entries_from, translate_npm_config_env};
#[cfg(test)]
use load::{
    expand_userconfig_path, load_npmrc_entries_tagged_with_home, load_npmrc_entries_with_home,
    userconfig_override_from_env,
};
#[cfg(test)]
use npmrc::{parse_npmrc, parse_npmrc_untrusted, substitute_env};
#[cfg(test)]
use token::sanitize_token_helper;
#[cfg(test)]
use url::{is_public_npmjs_url, normalize_registry_url, package_scope, registry_uri_key};
