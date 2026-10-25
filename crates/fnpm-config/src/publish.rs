//! `package.json#publishConfig` (bun `Options::apply_publish_config`): `registry` and
//! `@scope:registry` replace the registries for the publish, `tag` / `access` /
//! `provenance` are returned for the publish command, where flags take precedence.

use serde_json::Value;

use crate::auth::{self, CredentialSite, RegistrySource, Scope, build_scope};
use crate::npmrc::npm_config_bool;
use crate::{Config, Env, Error, RegistryScope, Result};

/// What `publishConfig` says besides the registries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishSettings {
    /// `publishConfig.tag`.
    pub tag: Option<String>,
    /// `publishConfig.access`: `public` or `restricted`.
    pub access: Option<String>,
    /// `publishConfig.provenance`, with npm's boolean coercion.
    pub provenance: Option<bool>,
}

/// `publishConfig[key]` as an http(s) URL. Any other value is an error, so that nothing is
/// published to the wrong place.
fn registry_url<'a>(config: &'a Value, key: &str) -> Result<Option<&'a str>> {
    let Some(value) = config.get(key) else {
        return Ok(None);
    };
    match value.as_str() {
        Some(url) if url.starts_with("https://") || url.starts_with("http://") => Ok(Some(url)),
        other => {
            let got = other
                .map(|v| format!(": {:?}", crate::redact_url(v)))
                .unwrap_or_default();
            Err(Error::InvalidValue {
                key: format!("publishConfig.{key}"),
                message: format!(
                    "invalid `{key}` value in `publishConfig`{got}, expected a URL starting with 'https://' or 'http://'"
                ),
            })
        }
    }
}

fn to_scope(r: &RegistryScope) -> Scope {
    Scope {
        url: r.url.clone(),
        auth: r.auth.clone(),
        declared_in_npmrc: false,
        url_auth_key: None,
    }
}

impl Config {
    /// The scope for a `publishConfig` URL replacing `current`. No environment variable is
    /// expanded into it (it comes from package data), and `.npmrc` lines only apply over https.
    fn publish_scope(
        &self,
        name: &str,
        current: &RegistryScope,
        url: &str,
    ) -> Result<RegistryScope> {
        let mut warnings = Vec::new();
        let source = RegistrySource::from_url(url);
        let has_userinfo = source.has_credentials();
        let mut scope = build_scope(
            CredentialSite::Registry(name),
            source,
            &Env::default(),
            false,
            &mut warnings,
        )?;
        let done = has_userinfo && scope.auth.is_some();
        if !done && !scope.auth.is_some() {
            let configured = std::iter::once(&self.registry)
                .chain(self.scopes.values())
                .find(|c| {
                    c.url.trim_end_matches('/') == scope.url.trim_end_matches('/')
                        && c.auth.is_some()
                });
            if let Some(c) = configured {
                scope.auth = c.auth.clone();
            } else if self.inherits(&to_scope(current), &scope.url) {
                scope.auth = current.auth.clone();
            }
        }
        if auth::is_https(&scope.url)
            && !scope.auth.is_some()
            && let Some((_, a)) = auth::find_entry(&self.url_auth, &scope.url)
        {
            scope.auth = a.clone();
        }
        let mut out = RegistryScope::new(&scope.url);
        out.auth = scope.auth;
        Ok(out)
    }

    fn inherits(&self, old: &Scope, new_url: &str) -> bool {
        auth::lenient_host(new_url) == auth::lenient_host(&old.url)
            && (auth::is_https(new_url) || !auth::is_https(&old.url))
            && auth::path_segments(new_url).starts_with(&auth::path_segments(&old.url))
            && match auth::find_entry(&self.url_auth, new_url) {
                Some((own, _)) => {
                    auth::find_entry(&self.url_auth, &old.url).is_some_and(|(k, _)| k == own)
                }
                None => true,
            }
    }

    /// Applies `package_json.publishConfig` for publishing `package_name`. Like npm,
    /// `registry` only replaces the default registry (not when `registry_from_cli`), and
    /// `@scope:registry` only the package's scope.
    pub fn apply_publish_config(
        &mut self,
        package_json: &Value,
        package_name: &str,
        registry_from_cli: bool,
    ) -> Result<PublishSettings> {
        let Some(config) = package_json.get("publishConfig") else {
            return Ok(PublishSettings::default());
        };
        let mut settings = PublishSettings {
            tag: config.get("tag").and_then(Value::as_str).map(str::to_owned),
            ..Default::default()
        };
        if let Some(access) = config.get("access").and_then(Value::as_str) {
            if access != "public" && access != "restricted" {
                return Err(Error::InvalidValue {
                    key: "publishConfig.access".to_owned(),
                    message: format!("invalid `access` value: '{access}'"),
                });
            }
            settings.access = Some(access.to_owned());
        }
        settings.provenance = config.get("provenance").and_then(|v| match v {
            Value::Bool(b) => Some(*b),
            Value::Null => Some(false),
            Value::Number(n) => Some(n.as_f64().is_some_and(|n| n != 0.0)),
            Value::Object(_) => Some(true),
            Value::String(s) => Some(npm_config_bool(s)),
            Value::Array(_) => None,
        });
        if !registry_from_cli && let Some(url) = registry_url(config, "registry")? {
            let current = self.registry.clone();
            self.registry = self.publish_scope("", &current, url)?;
        }
        if let Some((scope, _)) = package_name
            .strip_prefix('@')
            .and_then(|r| r.split_once('/'))
            && let Some(url) = registry_url(config, &format!("@{scope}:registry"))?
        {
            let current = self.registry_for(package_name).clone();
            let replaced = self.publish_scope(scope, &current, url)?;
            self.scopes.insert(format!("@{scope}"), replaced);
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Auth;

    fn cfg() -> Config {
        let mut c = Config::defaults(std::path::Path::new("/p"));
        c.url_auth.push((
            "r.example/".to_owned(),
            Auth::Token {
                token: "R".to_owned(),
            },
        ));
        c
    }

    #[test]
    fn registry_and_scope() {
        let mut c = cfg();
        let pkg = serde_json::json!({ "publishConfig": {
            "registry": "https://r.example/", "@acme:registry": "https://a.example/npm", "tag": "next", "access": "public", "provenance": "true"
        }});
        let s = c.apply_publish_config(&pkg, "@acme/x", false).unwrap();
        assert_eq!(
            s,
            PublishSettings {
                tag: Some("next".into()),
                access: Some("public".into()),
                provenance: Some(true)
            }
        );
        assert_eq!(c.registry.url, "https://r.example/");
        assert_eq!(c.registry.auth, Auth::Token { token: "R".into() });
        assert_eq!(c.registry_for("@acme/x").url, "https://a.example/npm/");
        let mut c = cfg();
        c.apply_publish_config(&pkg, "plain", true).unwrap();
        assert_eq!(c.registry.url, crate::DEFAULT_REGISTRY);
        assert!(c.scopes.is_empty());
    }

    #[test]
    fn http_gets_no_npmrc_line_and_no_env_expansion() {
        let mut c = cfg();
        c.apply_publish_config(
            &serde_json::json!({ "publishConfig": { "registry": "http://r.example/" }}),
            "x",
            false,
        )
        .unwrap();
        assert_eq!(c.registry.auth, Auth::None);
        let mut c = cfg();
        let pkg = serde_json::json!({ "publishConfig": { "registry": "https://r.example/:_authToken=$NPM_TOKEN" }});
        c.apply_publish_config(&pkg, "x", false).unwrap();
        assert_eq!(
            c.registry.auth,
            Auth::Token {
                token: "$NPM_TOKEN".into()
            }
        );
        assert_eq!(c.registry.url, "https://r.example/");
    }

    #[test]
    fn invalid_values() {
        let mut c = cfg();
        let e = c
            .apply_publish_config(
                &serde_json::json!({ "publishConfig": { "registry": "ftp://u:p@x/" }}),
                "x",
                false,
            )
            .unwrap_err();
        assert!(
            e.to_string().contains("expected a URL starting with")
                && !e.to_string().contains("u:p"),
            "{e}"
        );
        let e = c
            .apply_publish_config(
                &serde_json::json!({ "publishConfig": { "access": "secret" }}),
                "x",
                false,
            )
            .unwrap_err();
        assert!(e.to_string().contains("invalid `access` value: 'secret'"));
        assert_eq!(
            c.apply_publish_config(&serde_json::json!({}), "x", false)
                .unwrap(),
            PublishSettings::default()
        );
    }
}
