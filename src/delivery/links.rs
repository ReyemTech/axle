//! Linking a task to its PR: the project's repo, PR URL validation, and short-id matching.

use std::fmt;

/// The longest PR URL accepted, as for signal URLs.
const MAX_URL_LEN: usize = 2048;

/// A repository, `owner/name`, always lowercase (GitHub compares them case-insensitively).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    /// Parses `owner/name`; exactly two non-empty segments.
    pub fn parse(s: &str) -> Option<RepoRef> {
        let (owner, name) = s.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(RepoRef {
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
        })
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// The host part of a project key (`github.com` in `github.com/acme/web`).
fn key_host(key: &str) -> &str {
    key.split('/').next().unwrap_or_default()
}

/// A project's repository: the first two path segments after the host, lowercased, so
/// `github.com/acme/mono/web` is `acme/mono`. `None` when the key has fewer.
pub fn repo_of_key(key: &str) -> Option<RepoRef> {
    let mut parts = key.split('/');
    parts.next().filter(|host| !host.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(RepoRef {
        owner: owner.to_ascii_lowercase(),
        name: name.to_ascii_lowercase(),
    })
}

/// Whether the project key's host is exactly `github.com`.
pub fn is_github_key(key: &str) -> bool {
    key_host(key) == "github.com"
}

/// A validated PR URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrUrl {
    /// A GitHub pull request in the project's repo; `url` is canonical,
    /// `https://github.com/<owner>/<repo>/pull/<n>`.
    Github {
        repo: RepoRef,
        number: u32,
        url: String,
    },
    /// An https URL on a non-GitHub key's host, stored as a link only.
    Link { url: String },
}

/// Validates a reported PR URL against the project's key. A GitHub key takes only
/// `https://github.com/<owner>/<repo>/pull/<n>` (anything after `<n>` dropped) in the
/// project's repo, `n` in `1..2^31`; any other key takes an https URL on the key's host.
/// Userinfo and ports are refused everywhere. The error is the message shown to the agent.
pub fn parse_pr_url(raw: &str, project_key: &str) -> Result<PrUrl, String> {
    let host = key_host(project_key);
    let expected = repo_of_key(project_key);
    let target = match &expected {
        Some(repo) => format!("{host}/{repo}"),
        None => project_key.to_string(),
    };
    let refuse = || format!("pr must be a pull request in {target}");

    let raw = raw.trim();
    if raw.is_empty()
        || raw.len() > MAX_URL_LEN
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(refuse());
    }
    let rest = raw
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
        .map(|_| &raw[8..])
        .ok_or_else(refuse)?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host.is_empty() || authority.contains(['@', ':']) || !authority.eq_ignore_ascii_case(host) {
        return Err(refuse());
    }
    if !is_github_key(project_key) {
        return Ok(PrUrl::Link {
            url: raw.to_string(),
        });
    }

    let expected = expected.ok_or_else(refuse)?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segments = path.split('/');
    let (Some(owner), Some(name), Some("pull"), Some(number)) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(refuse());
    };
    let repo = RepoRef {
        owner: owner.to_ascii_lowercase(),
        name: name.to_ascii_lowercase(),
    };
    if repo != expected {
        return Err(refuse());
    }
    let number = Some(number)
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| (1..=i32::MAX as u32).contains(n))
        .ok_or_else(refuse)?;
    let url = format!("https://github.com/{repo}/pull/{number}");
    Ok(PrUrl::Github { repo, number, url })
}

/// Every task short id (`axl-` and 8 hex digits) mentioned in `text`, lowercase, first
/// occurrence order, without duplicates. Matches case-insensitive
/// `(^|[^a-z0-9])axl-([0-9a-f]{8})([^0-9a-f]|$)`.
pub fn find_short_ids(text: &str) -> Vec<String> {
    const PREFIX: &[u8] = b"axl-";
    const ID_LEN: usize = PREFIX.len() + 8;
    let bytes = text.as_bytes();
    let is_hex = |b: u8| b.is_ascii_hexdigit();
    let mut found: Vec<String> = Vec::new();
    for start in 0..bytes.len().saturating_sub(ID_LEN - 1) {
        let candidate = &bytes[start..start + ID_LEN];
        let boundary_before = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let boundary_after = bytes.get(start + ID_LEN).is_none_or(|&b| !is_hex(b));
        if boundary_before
            && boundary_after
            && candidate[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
            && candidate[PREFIX.len()..].iter().all(|&b| is_hex(b))
        {
            let id = String::from_utf8_lossy(candidate).to_ascii_lowercase();
            if !found.contains(&id) {
                found.push(id);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
        }
    }

    #[test]
    fn repo_mapping() {
        assert_eq!(
            repo_of_key("github.com/acme/mono/web"),
            Some(repo("acme", "mono"))
        );
        assert_eq!(
            repo_of_key("github.com/Acme/Web-App").unwrap().to_string(),
            "acme/web-app"
        );
        assert!(is_github_key("github.com/acme/mono/web"));
        assert_eq!(repo_of_key("gitlab.example.com/a/b"), Some(repo("a", "b")));
        assert!(!is_github_key("gitlab.example.com/a/b"));
        assert!(!is_github_key("github.com.evil.example/a/b"));
        assert_eq!(repo_of_key("github.com/acme"), None);
        assert_eq!(repo_of_key("local"), None);
        assert_eq!(RepoRef::parse("Acme/Web"), Some(repo("acme", "web")));
        assert_eq!(RepoRef::parse("acme"), None);
        assert_eq!(RepoRef::parse("a/b/c"), None);
    }

    #[test]
    fn pr_url_cases() {
        let key = "github.com/acme/web-app";
        assert_eq!(
            parse_pr_url("https://github.com/Acme/Web-App/pull/12/files", key),
            Ok(PrUrl::Github {
                repo: repo("acme", "web-app"),
                number: 12,
                url: "https://github.com/acme/web-app/pull/12".into(),
            })
        );
        assert!(matches!(
            parse_pr_url("https://github.com/acme/web-app/pull/2147483647", key),
            Ok(PrUrl::Github {
                number: 2147483647,
                ..
            })
        ));
        let refused = Err("pr must be a pull request in github.com/acme/web-app".to_string());
        for bad in [
            "http://github.com/acme/web-app/pull/12",
            "https://u@github.com/acme/web-app/pull/12",
            "https://github.com:443/acme/web-app/pull/12",
            "https://github.com/acme/other/pull/12",
            "https://github.com/acme/web-app/pull/0",
            "https://github.com/acme/web-app/pull/2147483648",
            "https://github.com/acme/web-app/pull/99999999999999999999",
            "https://github.com/acme/web-app/issues/12",
            "https://github.com/acme/web-app/pull/12x",
            "https://github.com/acme/web-app/pull/",
            "https://gitlab.com/acme/web-app/pull/12",
            "not a url",
        ] {
            assert_eq!(parse_pr_url(bad, key), refused, "{bad}");
        }

        // A monorepo key validates against its repo, not the full key.
        assert!(
            parse_pr_url(
                "https://github.com/acme/mono/pull/3",
                "github.com/acme/mono/web"
            )
            .is_ok()
        );

        let other = "gitlab.example.com/a/b";
        assert_eq!(
            parse_pr_url("https://gitlab.example.com/a/b/-/merge_requests/4", other),
            Ok(PrUrl::Link {
                url: "https://gitlab.example.com/a/b/-/merge_requests/4".into()
            })
        );
        assert_eq!(
            parse_pr_url(
                "https://elsewhere.example.com/a/b/-/merge_requests/4",
                other
            ),
            Err("pr must be a pull request in gitlab.example.com/a/b".to_string())
        );
        assert!(parse_pr_url("http://gitlab.example.com/a/b/-/merge_requests/4", other).is_err());
        assert!(parse_pr_url("https://u@gitlab.example.com/a/b", other).is_err());
    }

    #[test]
    fn short_id_scan() {
        assert_eq!(
            find_short_ids("feat/axl-4deae8e3-rate-limit"),
            vec!["axl-4deae8e3"]
        );
        assert_eq!(
            find_short_ids("Rate limit (AXL-4DEAE8E3)"),
            vec!["axl-4deae8e3"]
        );
        assert_eq!(find_short_ids("axl-4deae8e3"), vec!["axl-4deae8e3"]);
        assert!(find_short_ids("xaxl-4deae8e3").is_empty());
        assert!(find_short_ids("axl-4deae8e3f").is_empty());
        assert!(find_short_ids("axl-4deae8e").is_empty());
        assert_eq!(
            find_short_ids("axl-4deae8e3 and axl-0000000a, again AXL-4deae8e3"),
            vec!["axl-4deae8e3", "axl-0000000a"]
        );
    }
}
