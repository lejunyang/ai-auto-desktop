//! Server-owned grants for browser file documents and internal origins.
//! These gate control operations, not the browser's own script/network access.

use crate::{error, Result};
use std::collections::BTreeSet;
use std::path::PathBuf;
use url::Url;

#[derive(Clone, Debug, Default)]
pub struct BrowserAccess {
    file_roots: Vec<PathBuf>,
    internal_origins: BTreeSet<String>,
}

fn internal_scheme(scheme: &str) -> bool {
    matches!(scheme, "chrome" | "edge" | "chrome-extension")
}

fn parse(raw: &str) -> Result<Url> {
    if raw.is_empty() || raw.len() > 2048 || raw.trim() != raw || raw.chars().any(char::is_control)
    {
        return Err(error("CONTROL.INVALID_REQUEST", "browser URL must be an absolute address of at most 2048 bytes without surrounding whitespace or control characters"));
    }
    Url::parse(raw).map_err(|_| {
        error(
            "CONTROL.INVALID_REQUEST",
            "browser URL must be absolute and valid",
        )
    })
}

fn internal_origin(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    (internal_scheme(url.scheme())
        && !host.is_empty()
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.'))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none())
    .then(|| format!("{}://{}", url.scheme(), host.to_ascii_lowercase()))
}

impl BrowserAccess {
    /// Resolve operator directories once and reject malformed grants at startup.
    /// No wildcard or whole-scheme grant exists for browser internal pages.
    pub fn new(file_roots: &[PathBuf], internal_origins: &[String]) -> Result<Self> {
        let mut policy = Self::default();
        for root in file_roots {
            let canonical = root.canonicalize().map_err(|_| {
                error(
                    "CONTROL.INVALID_REQUEST",
                    "browser file grant must name an existing accessible directory",
                )
            })?;
            if !canonical.is_dir() {
                return Err(error(
                    "CONTROL.INVALID_REQUEST",
                    "browser file grant must name a directory",
                ));
            }
            if !policy.file_roots.contains(&canonical) {
                policy.file_roots.push(canonical);
            }
        }
        for raw in internal_origins {
            let url = parse(raw)?;
            let origin = internal_origin(&url).filter(|_| {
                matches!(url.path(), "" | "/") && url.query().is_none() && url.fragment().is_none()
            }).ok_or_else(|| error("CONTROL.INVALID_REQUEST", "browser internal grant must be an exact chrome://host, edge://host or chrome-extension://id origin without a path, credentials, port, query or fragment"))?;
            policy.internal_origins.insert(origin);
        }
        Ok(policy)
    }

    /// Navigation is explicit; script/data/blob URLs are never accepted here.
    /// File aliases are resolved to their canonical file URL before dispatch.
    pub fn navigation_url(&self, raw: &str) -> Result<String> {
        self.authorize_url(parse(raw)?)
    }

    fn authorize_url(&self, mut url: Url) -> Result<String> {
        match url.scheme() {
            "http" | "https" if url.host_str().is_some() => Ok(url.to_string()),
            "file" => {
                let file = self.file(&url)?;
                let mut canonical = Url::from_file_path(file).map_err(|_| {
                    error(
                        "CONTROL.INVALID_REQUEST",
                        "browser file path cannot be represented as a URL",
                    )
                })?;
                canonical.set_query(url.query());
                canonical.set_fragment(url.fragment());
                Ok(canonical.to_string())
            }
            scheme if internal_scheme(scheme) => {
                let origin = internal_origin(&url).ok_or_else(|| {
                    error("CONTROL.INVALID_REQUEST", "invalid browser internal URL")
                })?;
                if !self.internal_origins.contains(&origin) {
                    return Err(error(
                        "POLICY.DENIED",
                        "browser internal origin was not granted by the server operator",
                    ));
                }
                // Chromium internal hosts are ASCII and case insensitive.
                let host = url.host_str().unwrap().to_ascii_lowercase();
                url.set_host(Some(&host)).map_err(|_| {
                    error("CONTROL.INVALID_REQUEST", "invalid browser internal host")
                })?;
                Ok(url.to_string())
            }
            _ => Err(error(
                "CONTROL.INVALID_REQUEST",
                "navigation requires an http(s) address or an explicitly granted file/internal URL",
            )),
        }
    }

    /// Apply grants to the current document as well as the requested destination.
    /// Blank/srcdoc are inert document addresses; callers must also validate
    /// their embedding frame chain before exposing or operating on a child.
    pub fn check_document(&self, raw: &str) -> Result<()> {
        // Current browser URLs can legitimately exceed the action-input limit
        // (e.g. OAuth redirects and data frames). Do not revoke their origin.
        let url = Url::parse(raw)
            .map_err(|_| error("POLICY.DENIED", "browser document has no valid current URL"))?;
        if url.scheme() == "about"
            && matches!(url.path(), "blank" | "srcdoc")
            && url.query().is_none()
        {
            return Ok(());
        }
        // Existing data documents carry no file/internal origin privileges.
        // They remain observable; requesting data navigation is still refused.
        if url.scheme() == "data" {
            return Ok(());
        }
        if url.scheme() == "blob" {
            let inner = Url::parse(url.path())
                .map_err(|_| error("POLICY.DENIED", "blob document has no verifiable origin"))?;
            if matches!(inner.scheme(), "http" | "https") && inner.host_str().is_some() {
                return Ok(());
            }
            return Err(error(
                "POLICY.DENIED",
                "opaque or privileged blob document origin cannot be authorized",
            ));
        }
        self.authorize_url(url).map(|_| ())
    }

    fn file(&self, url: &Url) -> Result<PathBuf> {
        if self.file_roots.is_empty() {
            return Err(error(
                "POLICY.DENIED",
                "browser file access was not granted by the server operator",
            ));
        }
        // Never turn a URL into a remote filesystem/UNC access, on any platform.
        if url
            .host_str()
            .is_some_and(|host| !host.is_empty() && host != "localhost")
        {
            return Err(error(
                "POLICY.DENIED",
                "browser file grants permit only local files",
            ));
        }
        let path = url
            .to_file_path()
            .map_err(|_| error("CONTROL.INVALID_REQUEST", "invalid local file URL"))?;
        let canonical = path.canonicalize().map_err(|_| {
            error("POLICY.DENIED", "browser file must be an existing accessible regular file within a granted directory")
        })?;
        if !canonical.is_file()
            || !self
                .file_roots
                .iter()
                .any(|root| canonical.starts_with(root))
        {
            return Err(error(
                "POLICY.DENIED",
                "browser file is outside the granted directories or is not a regular file",
            ));
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_navigation_only_accepts_absolute_web_addresses() {
        let policy = BrowserAccess::default();
        for url in ["http://127.0.0.1/", "https://example.test/a?b#c"] {
            assert!(policy.navigation_url(url).is_ok());
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,hello",
            "blob:https://example.test/id",
            "about:blank",
            "/relative",
            " https://example.test",
            "https://example.test/\n",
        ] {
            assert!(policy.navigation_url(url).is_err(), "{url}");
        }
        for url in [
            "file:///etc/passwd",
            "chrome://settings/",
            "edge://settings/",
        ] {
            assert_eq!(
                policy.navigation_url(url).unwrap_err().code,
                "POLICY.DENIED"
            );
        }
        assert!(policy.check_document("about:blank").is_ok());
        assert!(policy.check_document("about:srcdoc").is_ok());
        assert!(policy
            .check_document("blob:https://example.test/id")
            .is_ok());
        assert!(policy.check_document("blob:null/id").is_err());
        assert!(policy.check_document("about:config").is_err());
        let long_web = format!("https://example.test/?state={}", "a".repeat(4096));
        assert!(policy.check_document(&long_web).is_ok());
        assert!(policy.navigation_url(&long_web).is_err());
        assert!(policy
            .check_document(&format!("data:text/html,{}", "a".repeat(4096)))
            .is_ok());
    }

    #[test]
    fn internal_grant_is_an_exact_origin_with_paths_allowed() {
        let policy = BrowserAccess::new(&[], &["chrome://settings/".into()]).unwrap();
        for url in [
            "chrome://settings",
            "chrome://Settings/appearance?search=theme#page",
        ] {
            assert!(policy.navigation_url(url).is_ok(), "{url}");
        }
        for url in [
            "edge://settings/",
            "chrome://settings.evil/",
            "chrome://extensions/",
            "chrome://user@settings/",
            "chrome://settings:80/",
        ] {
            assert!(policy.navigation_url(url).is_err(), "{url}");
        }
        for grant in [
            "chrome://*",
            "chrome://settings/appearance",
            "chrome://settings?x",
            "chrome://settings#x",
            "https://example.test",
            "file:///tmp",
            "devtools://devtools",
        ] {
            assert!(BrowserAccess::new(&[], &[grant.into()]).is_err(), "{grant}");
        }
    }

    #[test]
    fn file_grants_use_directory_components_and_canonical_files() {
        let root =
            std::env::temp_dir().join(format!("aad-browser-access-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("allowed")).unwrap();
        std::fs::create_dir_all(root.join("allowed-other")).unwrap();
        let file = root.join("allowed/中文 report.html");
        let outside = root.join("allowed-other/outside.html");
        std::fs::write(&file, "synthetic fixture").unwrap();
        std::fs::write(&outside, "outside fixture").unwrap();
        let policy = BrowserAccess::new(&[root.join("allowed")], &[]).unwrap();
        let url = Url::from_file_path(&file).unwrap();
        assert_eq!(policy.navigation_url(url.as_str()).unwrap(), url.as_str());
        assert!(policy.check_document(url.as_str()).is_ok());
        assert!(policy
            .navigation_url(Url::from_file_path(&outside).unwrap().as_str())
            .is_err());
        assert!(policy
            .navigation_url(
                Url::from_directory_path(root.join("allowed"))
                    .unwrap()
                    .as_str()
            )
            .is_err());
        assert!(policy
            .navigation_url("file://remote-server/share/page.html")
            .is_err());
        #[cfg(unix)]
        {
            let link = root.join("allowed/link.html");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(policy
                .navigation_url(Url::from_file_path(&link).unwrap().as_str())
                .is_err());
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert_eq!(
                policy
                    .navigation_url(Url::from_file_path(&link).unwrap().as_str())
                    .unwrap(),
                url.as_str()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
