use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct GithubRepo {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct GithubPull {
    pub number: u64,
    pub draft: bool,
    pub head_sha: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub author: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GithubFile {
    pub filename: String,
    pub status: String,
    pub raw_url: Option<String>,
    pub previous_filename: Option<String>,
}

#[derive(Clone)]
pub struct GithubClient {
    client: Client,
    repo: GithubRepo,
}

impl GithubClient {
    pub fn new(repo: GithubRepo, token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("dossiers-cli"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|err| anyhow!("invalid github token header: {err}"))?,
        );

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .context("building GitHub client")?;

        Ok(Self { client, repo })
    }

    pub fn repo(&self) -> &GithubRepo {
        &self.repo
    }

    pub fn list_open_pulls(&self) -> Result<Vec<GithubPull>> {
        let mut pulls = Vec::new();
        let mut page = 1u32;

        loop {
            let url = self.api_url("pulls");
            let response = self
                .client
                .get(url)
                .query(&[
                    ("state", "open"),
                    ("per_page", "50"),
                    ("page", &page.to_string()),
                ])
                .send()
                .context("requesting open pull requests")?;
            let page_pulls: Vec<PullResponse> = parse_json(response)?;
            let count = page_pulls.len();
            pulls.extend(page_pulls.into_iter().map(|pull| GithubPull {
                number: pull.number,
                draft: pull.draft,
                head_sha: pull.head.sha,
                created_at: parse_timestamp(&pull.created_at),
                updated_at: parse_timestamp(&pull.updated_at),
                author: pull.user.map(|u| u.login),
            }));

            if count < 50 {
                break;
            }
            page += 1;
        }

        Ok(pulls)
    }

    pub fn list_pull_files(&self, pull_number: u64) -> Result<Vec<GithubFile>> {
        let mut files = Vec::new();
        let mut page = 1u32;

        loop {
            let url = self.api_url(&format!("pulls/{pull_number}/files"));
            let response = self
                .client
                .get(url)
                .query(&[("per_page", "100"), ("page", &page.to_string())])
                .send()
                .with_context(|| format!("requesting files for PR #{pull_number}"))?;
            let page_files: Vec<FileResponse> = parse_json(response)?;
            let count = page_files.len();
            files.extend(page_files.into_iter().map(|file| GithubFile {
                filename: file.filename,
                status: file.status,
                raw_url: file.raw_url,
                previous_filename: file.previous_filename,
            }));

            if count < 100 {
                break;
            }
            page += 1;
        }

        Ok(files)
    }

    pub fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("downloading {url}"))?
            .error_for_status()
            .with_context(|| format!("downloading {url}"))?;
        let bytes = response
            .bytes()
            .with_context(|| format!("reading bytes from {url}"))?;
        Ok(bytes.to_vec())
    }

    pub fn download_file_at_ref(&self, path: &str, reference: &str) -> Result<Vec<u8>> {
        let url = self.api_url(&format!("contents/{path}"));
        let response = self
            .client
            .get(url)
            .query(&[("ref", reference)])
            .send()
            .with_context(|| format!("requesting contents for {path} at {reference}"))?
            .error_for_status()
            .with_context(|| format!("requesting contents for {path} at {reference}"))?;

        let content: ContentResponse = response.json().with_context(|| {
            format!("parsing content metadata for {path} at reference {reference}")
        })?;

        let Some(download_url) = content.download_url else {
            anyhow::bail!("no download url for {path} at {reference}")
        };

        self.download_bytes(&download_url)
    }

    fn api_url(&self, path: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/{}",
            self.repo.owner,
            self.repo.name,
            path.trim_start_matches('/')
        )
    }
}

pub fn parse_github_repo(raw: &str) -> Option<GithubRepo> {
    let cleaned = raw.trim().trim_end_matches(".git");
    if cleaned.is_empty() {
        return None;
    }

    let repo_part = if let Some(stripped) = cleaned.strip_prefix("git@github.com:") {
        stripped
    } else if let Some(stripped) = cleaned.strip_prefix("github.com:") {
        stripped
    } else if let Some(stripped) = cleaned.strip_prefix("ssh://git@github.com/") {
        stripped
    } else if let Some(stripped) = cleaned.strip_prefix("ssh://github.com/") {
        stripped
    } else if let Some(stripped) = cleaned.strip_prefix("git://github.com/") {
        stripped
    } else if let Some(stripped) = parse_http_github_repo(cleaned) {
        stripped
    } else if cleaned.contains('/') && !cleaned.contains(':') {
        cleaned
    } else {
        return None;
    };

    let mut segments = repo_part.trim_matches('/').split('/');
    let owner = segments.next()?.trim();
    let name = segments.next()?.trim();
    if owner.is_empty() || name.is_empty() {
        return None;
    }

    Some(GithubRepo {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn parse_http_github_repo(cleaned: &str) -> Option<&str> {
    let rest = cleaned
        .strip_prefix("https://")
        .or_else(|| cleaned.strip_prefix("http://"))?;
    let slash = rest.find('/')?;
    let (authority, path) = rest.split_at(slash);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host != "github.com" {
        return None;
    }
    let path = &path[1..];
    if path.is_empty() {
        return None;
    }
    Some(path)
}

fn parse_json<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().unwrap_or_default();
        anyhow::bail!("GitHub API error ({status}): {text}");
    }
    response
        .json::<T>()
        .context("parsing GitHub API response body")
}

#[derive(Debug, Deserialize)]
struct PullResponse {
    number: u64,
    draft: bool,
    head: HeadRef,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    user: Option<UserRef>,
}

#[derive(Debug, Deserialize)]
struct HeadRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct FileResponse {
    filename: String,
    status: String,
    raw_url: Option<String>,
    previous_filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentResponse {
    download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserRef {
    login: String,
}

fn parse_timestamp(raw: &str) -> i64 {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|_| Utc::now().timestamp_millis())
}
