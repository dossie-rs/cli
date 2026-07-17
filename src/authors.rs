//! Producer-side resolution of spec authors into rich identities (name +
//! avatar + profile URL).
//!
//! A spec's authors come from its `authors:` frontmatter or, when it declares
//! none, from the git author of the commit that first added it. Avatars come
//! from the author's linked GitHub account — resolved from the addition commit
//! SHA, or matched by email against accounts seen on other commits — and fall
//! back to a GitHub-noreply-derived avatar or a Gravatar computed from the
//! email. Every lookup is best-effort: with no GitHub token/repo the local
//! fallbacks still yield an avatar for any address.

use std::collections::HashMap;

use crate::bundle::Author;
use crate::git_utils::GitAuthor;
use crate::github::{CommitIdentity, GithubClient};

/// The inputs needed to resolve one author: the display name plus the optional
/// email and addition-commit SHA that drive the avatar lookup.
#[derive(Debug, Clone)]
pub struct AuthorSeed {
    pub name: String,
    pub email: Option<String>,
    pub commit_sha: Option<String>,
}

/// A GitHub account resolved for an author: its avatar and profile URL.
#[derive(Debug, Clone)]
struct GitHubIdentity {
    avatar_url: String,
    profile_url: String,
}

/// Best-effort GitHub identity lookups for a push. All lookups miss when no
/// token/repo is available, leaving only the local Gravatar / noreply
/// fallbacks (as used by offline static builds via [`AuthorResolver::local`]).
#[derive(Debug, Default)]
pub struct AuthorResolver {
    by_sha: HashMap<String, CommitIdentity>,
    by_email: HashMap<String, GitHubIdentity>,
}

impl AuthorResolver {
    /// A resolver with no GitHub data — used offline (e.g. static builds).
    pub fn local() -> Self {
        Self::default()
    }

    /// Resolve every unique addition-commit SHA across `seeds` via the GitHub
    /// commits API (deduplicated, one call per SHA), building a sha→identity
    /// cache plus an email→GitHub-account map that other specs' frontmatter
    /// authors can be matched against. GitHub failures are logged and skipped.
    pub fn from_github<'a>(
        client: &GithubClient,
        seeds: impl Iterator<Item = &'a AuthorSeed>,
    ) -> Self {
        let mut by_sha: HashMap<String, CommitIdentity> = HashMap::new();
        for seed in seeds {
            if let Some(sha) = seed.commit_sha.as_deref() {
                if by_sha.contains_key(sha) {
                    continue;
                }
                match client.get_commit(sha) {
                    Ok(identity) => {
                        by_sha.insert(sha.to_string(), identity);
                    }
                    Err(err) => {
                        eprintln!(
                            "Warning: could not resolve commit {sha} for author avatar: {err}"
                        );
                        by_sha.insert(sha.to_string(), CommitIdentity::default());
                    }
                }
            }
        }

        let mut by_email: HashMap<String, GitHubIdentity> = HashMap::new();
        for identity in by_sha.values() {
            if let (Some(email), Some(gh)) = (identity.email.as_deref(), github_identity(identity))
            {
                by_email.entry(email.trim().to_lowercase()).or_insert(gh);
            }
        }

        Self { by_sha, by_email }
    }
}

/// Build the resolution seeds for a spec. Frontmatter authors override the
/// automatic detection; only when a spec declares none do we fall back to the
/// git author of the commit that first added it.
pub fn build_seeds(
    raw_frontmatter_authors: &[String],
    git_author: Option<&GitAuthor>,
) -> Vec<AuthorSeed> {
    let frontmatter = seeds_from_names(raw_frontmatter_authors);
    if !frontmatter.is_empty() {
        return frontmatter;
    }
    if let Some(git) = git_author {
        let name = if git.name.trim().is_empty() {
            git.email.trim().to_string()
        } else {
            git.name.trim().to_string()
        };
        if !name.is_empty() {
            return vec![AuthorSeed {
                name,
                email: non_empty(&git.email),
                commit_sha: non_empty(&git.commit_sha),
            }];
        }
    }
    Vec::new()
}

/// Seeds from a plain list of author display names (each may embed `<email>`),
/// for inputs that carry no git history (e.g. JSON specs).
pub fn seeds_from_names(names: &[String]) -> Vec<AuthorSeed> {
    names
        .iter()
        .filter_map(|raw| {
            let (name, email) = split_name_email(raw);
            if name.is_empty() {
                None
            } else {
                Some(AuthorSeed {
                    name,
                    email,
                    commit_sha: None,
                })
            }
        })
        .collect()
}

/// Resolve seeds to rich authors, attaching an avatar to each where possible.
pub fn resolve_authors(seeds: &[AuthorSeed], resolver: &AuthorResolver) -> Vec<Author> {
    seeds
        .iter()
        .map(|seed| resolve_one(seed, resolver))
        .collect()
}

fn resolve_one(seed: &AuthorSeed, resolver: &AuthorResolver) -> Author {
    // 1. The author's own addition commit resolved to a GitHub account.
    if let Some(sha) = seed.commit_sha.as_deref() {
        if let Some(gh) = resolver.by_sha.get(sha).and_then(github_identity) {
            return with_avatar(&seed.name, gh);
        }
    }
    if let Some(email) = seed
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        // 2. Match this author's email against a GitHub account seen on any commit.
        if let Some(gh) = resolver.by_email.get(&email.to_lowercase()) {
            return with_avatar(&seed.name, gh.clone());
        }
        // 3. A GitHub noreply email encodes the account — derive it offline.
        if let Some((avatar_url, profile_url)) = github_from_noreply(email) {
            return Author {
                name: seed.name.clone(),
                avatar_url: Some(avatar_url),
                url: Some(profile_url),
            };
        }
        // 4. Fall back to a Gravatar (an identicon when the address has none).
        return Author {
            name: seed.name.clone(),
            avatar_url: Some(gravatar_url(email)),
            url: None,
        };
    }
    // 5. No email at all — name only.
    Author {
        name: seed.name.clone(),
        avatar_url: None,
        url: None,
    }
}

fn with_avatar(name: &str, gh: GitHubIdentity) -> Author {
    Author {
        name: name.to_string(),
        avatar_url: Some(gh.avatar_url),
        url: Some(gh.profile_url),
    }
}

fn github_identity(identity: &CommitIdentity) -> Option<GitHubIdentity> {
    let avatar_url = identity.avatar_url.clone().filter(|s| !s.is_empty())?;
    let login = identity.login.clone().filter(|s| !s.is_empty());
    let profile_url = identity
        .html_url
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| login.map(|l| format!("https://github.com/{l}")))
        .unwrap_or_default();
    Some(GitHubIdentity {
        avatar_url,
        profile_url,
    })
}

/// Split `"Name <email>"` into a trimmed name and the address (if present).
/// A bare `"<email>"` uses the address as the name.
pub fn split_name_email(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    if let (Some(open), Some(close)) = (raw.find('<'), raw.rfind('>')) {
        if open < close {
            let email = raw[open + 1..close].trim().to_string();
            let name = raw[..open].trim().to_string();
            let name = if name.is_empty() { email.clone() } else { name };
            return (name, non_empty(&email));
        }
    }
    (raw.to_string(), None)
}

/// GitHub serves an account's avatar at `github.com/<login>.png`, and its
/// noreply commit email encodes the login
/// (`<login>@users.noreply.github.com` or
/// `<id>+<login>@users.noreply.github.com`).
fn github_from_noreply(email: &str) -> Option<(String, String)> {
    let local = email.trim().to_lowercase();
    let user = local.strip_suffix("@users.noreply.github.com")?;
    let login = user.rsplit('+').next().unwrap_or(user);
    if login.is_empty() {
        return None;
    }
    Some((
        format!("https://github.com/{login}.png?size=160"),
        format!("https://github.com/{login}"),
    ))
}

/// Gravatar image URL for an email, falling back to a generated identicon when
/// the address has no Gravatar so an avatar always renders.
pub fn gravatar_url(email: &str) -> String {
    let digest = md5::compute(email.trim().to_lowercase().as_bytes());
    format!("https://www.gravatar.com/avatar/{digest:x}?d=identicon&s=160")
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_email_extracts_address() {
        assert_eq!(
            split_name_email("Alice <alice@example.com>"),
            ("Alice".into(), Some("alice@example.com".into()))
        );
        assert_eq!(split_name_email("Bob"), ("Bob".into(), None));
        assert_eq!(
            split_name_email("<solo@example.com>"),
            ("solo@example.com".into(), Some("solo@example.com".into()))
        );
    }

    #[test]
    fn frontmatter_overrides_git_author() {
        let git = GitAuthor {
            name: "Committer".into(),
            email: "committer@example.com".into(),
            commit_sha: "abc".into(),
        };
        let seeds = build_seeds(&["Declared <declared@example.com>".into()], Some(&git));
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].name, "Declared");
        assert_eq!(seeds[0].email.as_deref(), Some("declared@example.com"));
        assert!(seeds[0].commit_sha.is_none());
    }

    #[test]
    fn falls_back_to_git_author_when_no_frontmatter() {
        let git = GitAuthor {
            name: "Committer".into(),
            email: "committer@example.com".into(),
            commit_sha: "abc".into(),
        };
        let seeds = build_seeds(&[], Some(&git));
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].name, "Committer");
        assert_eq!(seeds[0].commit_sha.as_deref(), Some("abc"));
    }

    #[test]
    fn noreply_email_yields_github_avatar_offline() {
        let seeds = vec![AuthorSeed {
            name: "Octo".into(),
            email: Some("583231+octocat@users.noreply.github.com".into()),
            commit_sha: None,
        }];
        let authors = resolve_authors(&seeds, &AuthorResolver::local());
        assert_eq!(
            authors[0].avatar_url.as_deref(),
            Some("https://github.com/octocat.png?size=160")
        );
        assert_eq!(
            authors[0].url.as_deref(),
            Some("https://github.com/octocat")
        );
    }

    #[test]
    fn plain_email_falls_back_to_gravatar() {
        let seeds = vec![AuthorSeed {
            name: "Alice".into(),
            email: Some("Alice@Example.com".into()),
            commit_sha: None,
        }];
        let authors = resolve_authors(&seeds, &AuthorResolver::local());
        // md5 of the lowercased, trimmed address.
        assert_eq!(
            authors[0].avatar_url.as_deref(),
            Some("https://www.gravatar.com/avatar/c160f8cc69a4f0bf2b0362752353d060?d=identicon&s=160")
        );
        assert!(authors[0].url.is_none());
    }
}
