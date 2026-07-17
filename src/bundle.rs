//! Wire format for Dossiers content packages.
//!
//! A package describes the contents of a project at a particular point in time:
//! a mainline snapshot of every spec (source bytes + binary assets) plus zero
//! or more sparse PR change-sets that add, update, or remove specs and assets
//! against that mainline.
//!
//! Packages are transmitted as zip archives with a fixed layout:
//!
//! ```text
//! manifest.json
//! project.toml                (optional)
//! main/<dir>/<file>
//! prs/<n>/meta.json
//! prs/<n>/specs/<dir>/<file>
//! ```
//!
//! See `specs/0007-api-server` § Package format for the authoritative
//! description.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const PACKAGE_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: Manifest,
    /// Verbatim bytes of the project's `dossiers.toml`, if any.
    pub project_config: Option<Vec<u8>>,
    pub mainline: Mainline,
    pub pr_changes: Vec<PrChangeSet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package_version: u32,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default = "default_source_mode")]
    pub source: SourceMode,
    /// Pre-computed index of every spec under `main/`. Carries the metadata
    /// consumers need to build a listing without parsing each source file.
    /// May be empty when produced by an older CLI; consumers should fall back
    /// to deriving fields from the source bytes.
    #[serde(default)]
    pub specs: Vec<SpecIndexEntry>,
    /// Base commit this package is a delta against. When set, the package is an
    /// incremental update against a prior sync: consumers upsert the specs/PRs
    /// present, apply the `deleted_*` removals, and prune nothing else. Absent
    /// for a full snapshot (the historical behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Mainline spec ids removed since `base_commit`. Delta mode only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_specs: Vec<String>,
    /// PR numbers whose revisions should be removed (closed PRs) since
    /// `base_commit`. Delta mode only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_prs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecIndexEntry {
    pub id: String,
    pub dir_name: String,
    /// Source file path relative to the spec directory, e.g. `"authentication.md"`.
    pub source_path: String,
    pub format: DocFormat,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// Producer-resolved rich author identities (display name + optional avatar
    /// and profile URL), parallel to `authors` and in the same order. `authors`
    /// remains the name-only list that drives slugs, search, and the author
    /// index; this carries the presentation data those consumers don't need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors_meta: Vec<Author>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
    /// Producer-resolved outbound links (the document's `links:` frontmatter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<MetaLink>,
    /// Producer-resolved, render-ready extra metadata rows (the configured
    /// `extra_metadata_fields`). Carries the display representation the raw
    /// `extra` map can't express; `extra` remains the machine-readable map.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<MetaField>,
}

/// A producer-resolved author identity: a display name plus, when resolved, an
/// avatar and a profile URL. Names come from the document's `authors:`
/// frontmatter or, failing that, the git commit that first added the spec.
/// Avatars come from the author's linked GitHub account (resolved via the
/// commits API) or fall back to a Gravatar derived from the commit email.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Author {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A producer-resolved outbound link, rendered under the "Links" metadata row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaLink {
    pub label: String,
    pub href: String,
}

/// A producer-resolved, render-ready extra metadata row. The producer applies
/// the project's field configuration (display name, type, link format) so
/// consumers render it verbatim without re-parsing the source or holding the
/// config. `value` is plain text unless `html` is set, in which case it is
/// trusted pre-rendered HTML (a Markdown-typed field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaField {
    pub label: String,
    pub value: String,
    /// When set, the value is wrapped in a link to this href.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    /// The `value` is trusted pre-rendered HTML rather than plain text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub html: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_source_mode() -> SourceMode {
    SourceMode::Push
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Push,
    Pull,
}

#[derive(Debug, Clone, Default)]
pub struct Mainline {
    pub specs: Vec<Spec>,
}

#[derive(Debug, Clone)]
pub struct Spec {
    /// Numeric prefix of the spec directory, e.g. `"0001"`.
    pub id: String,
    /// Directory name as it appears on disk, e.g. `"0001-authentication"`.
    pub dir_name: String,
    pub format: DocFormat,
    /// Source file path relative to the spec directory, e.g. `"authentication.md"`.
    pub source_path: String,
    /// Raw bytes of the source file.
    pub source: Vec<u8>,
    /// Other files alongside the spec (binary assets).
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DocFormat {
    Markdown,
    Asciidoc,
}

impl DocFormat {
    fn from_extension(path: &str) -> Option<Self> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => Some(Self::Markdown),
            "adoc" | "asciidoc" => Some(Self::Asciidoc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Asset {
    /// Path relative to the spec directory, e.g. `"images/flow.svg"`.
    pub path: String,
    /// Optional content-type hint. If absent, consumers may guess from the extension.
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PrChangeSet {
    pub pr_number: u64,
    pub branch: String,
    pub head_sha: String,
    /// PR title from GitHub.
    pub title: String,
    /// PR author login, if known.
    pub author: Option<String>,
    /// `"DRAFT"` for draft PRs, `"REVIEW"` otherwise.
    pub state: String,
    /// GitHub PR URL (`html_url`), for linking out from the rendered site.
    pub url: String,
    /// PR creation / last-update times.
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub spec_changes: Vec<SpecChange>,
    pub asset_changes: Vec<AssetChange>,
    /// Resolved metadata for each target spec the PR touches (title, status,
    /// authors, dates), computed by the producer so consumers don't re-parse
    /// the source. Keyed by spec id.
    pub spec_meta: Vec<PrSpecMeta>,
}

/// Producer-resolved metadata for one spec targeted by a PR change-set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSpecMeta {
    pub spec_id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// Rich author identities parallel to `authors` (see `SpecIndexEntry`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors_meta: Vec<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    /// Producer-resolved outbound links (the document's `links:` frontmatter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<MetaLink>,
    /// Producer-resolved, render-ready extra metadata rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<MetaField>,
}

#[derive(Debug, Clone)]
pub enum SpecChange {
    Upsert(Spec),
    Remove { id: String },
}

#[derive(Debug, Clone)]
pub enum AssetChange {
    Upsert { spec_id: String, asset: Asset },
    Remove { spec_id: String, path: String },
}

#[derive(Error, Debug)]
pub enum BundleError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid package: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrMetaWire {
    pr_number: u64,
    branch: String,
    head_sha: String,
    #[serde(default)]
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    removed_specs: Vec<String>,
    #[serde(default)]
    removed_assets: Vec<RemovedAssetWire>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    spec_meta: Vec<PrSpecMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemovedAssetWire {
    spec_id: String,
    path: String,
}

impl Package {
    /// Serialise this package as a zip archive.
    pub fn write_zip<W: Write + Seek>(&self, writer: W) -> Result<W, BundleError> {
        let mut zw = ZipWriter::new(writer);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let manifest_json = serde_json::to_vec_pretty(&self.manifest)?;
        zw.start_file("manifest.json", opts)?;
        zw.write_all(&manifest_json)?;

        if let Some(bytes) = &self.project_config {
            zw.start_file("project.toml", opts)?;
            zw.write_all(bytes)?;
        }

        for spec in &self.mainline.specs {
            write_spec(&mut zw, "main", spec, opts)?;
        }

        for pr in &self.pr_changes {
            let meta = PrMetaWire {
                pr_number: pr.pr_number,
                branch: pr.branch.clone(),
                head_sha: pr.head_sha.clone(),
                title: pr.title.clone(),
                author: pr.author.clone(),
                state: pr.state.clone(),
                url: pr.url.clone(),
                created_at: pr.created_at,
                updated_at: pr.updated_at,
                spec_meta: pr.spec_meta.clone(),
                removed_specs: pr
                    .spec_changes
                    .iter()
                    .filter_map(|c| match c {
                        SpecChange::Remove { id } => Some(id.clone()),
                        _ => None,
                    })
                    .collect(),
                removed_assets: pr
                    .asset_changes
                    .iter()
                    .filter_map(|c| match c {
                        AssetChange::Remove { spec_id, path } => Some(RemovedAssetWire {
                            spec_id: spec_id.clone(),
                            path: path.clone(),
                        }),
                        _ => None,
                    })
                    .collect(),
            };

            let prefix = format!("prs/{}", pr.pr_number);
            zw.start_file(format!("{prefix}/meta.json"), opts)?;
            zw.write_all(&serde_json::to_vec_pretty(&meta)?)?;

            let pr_specs_prefix = format!("{prefix}/specs");
            for change in &pr.spec_changes {
                if let SpecChange::Upsert(spec) = change {
                    write_spec(&mut zw, &pr_specs_prefix, spec, opts)?;
                }
            }

            for change in &pr.asset_changes {
                if let AssetChange::Upsert { spec_id, asset } = change {
                    let path = format!("{prefix}/specs/{spec_id}/{}", asset.path);
                    zw.start_file(&path, opts)?;
                    zw.write_all(&asset.bytes)?;
                }
            }
        }

        let writer = zw.finish()?;
        Ok(writer)
    }

    /// Build a `Mainline` snapshot by walking a directory of spec subdirectories.
    ///
    /// At the top level, only entries with a numeric prefix matching the
    /// `<digits>-name` convention are considered specs; everything else is
    /// silently ignored. Inside each spec directory, all files (including
    /// nested subdirectories) are included; hidden files (names starting with
    /// `.`) are skipped.
    pub fn mainline_from_directory(specs_dir: &Path) -> Result<Mainline, BundleError> {
        if !specs_dir.is_dir() {
            return Err(BundleError::Invalid(format!(
                "{} is not a directory",
                specs_dir.display()
            )));
        }
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        // A spec is either a subdirectory (`0001-name/…`) or a single flat
        // source file (`0001-name.md`). Both are keyed by an in-zip directory
        // name so `group_specs` treats them uniformly.
        let mut sources: Vec<(String, SpecSource)> = Vec::new();
        for entry in fs::read_dir(specs_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || extract_spec_id(&name).is_none() {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                sources.push((name, SpecSource::Dir(path)));
            } else if path.is_file() && DocFormat::from_extension(&name).is_some() {
                // Use the file stem as the directory name so `0001-name.md`
                // becomes spec `0001-name` with source `0001-name.md`.
                let dir_name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| stem.to_string())
                    .unwrap_or_else(|| name.clone());
                sources.push((dir_name, SpecSource::File { name, path }));
            }
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        for (dir_name, source) in sources {
            match source {
                SpecSource::Dir(dir_path) => visit_spec_dir(&dir_path, &dir_name, &mut entries)?,
                SpecSource::File { name, path } => {
                    let bytes = fs::read(&path)?;
                    entries.push((format!("{dir_name}/{name}"), bytes));
                }
            }
        }
        Ok(Mainline {
            specs: group_specs(entries)?,
        })
    }

    /// Decode a package from a zip archive.
    pub fn read_zip<R: Read + Seek>(reader: R) -> Result<Self, BundleError> {
        let mut archive = ZipArchive::new(reader)?;

        // Pull every entry into memory keyed by its in-zip path. Order is
        // preserved within a directory so callers see specs in insertion
        // order.
        let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(archive.len());
        for idx in 0..archive.len() {
            let mut entry = archive.by_index(idx)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            files.push((name, buf));
        }

        let manifest_bytes = take_file(&mut files, "manifest.json")
            .ok_or_else(|| BundleError::Invalid("missing manifest.json".into()))?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.package_version != PACKAGE_VERSION {
            return Err(BundleError::Invalid(format!(
                "unsupported package_version {}",
                manifest.package_version
            )));
        }

        let project_config = take_file(&mut files, "project.toml");

        let mut mainline_files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut pr_files: BTreeMap<u64, Vec<(String, Vec<u8>)>> = BTreeMap::new();
        let mut pr_meta: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

        for (name, bytes) in files {
            if let Some(rest) = name.strip_prefix("main/") {
                mainline_files.push((rest.to_string(), bytes));
                continue;
            }
            if let Some(rest) = name.strip_prefix("prs/") {
                let mut parts = rest.splitn(2, '/');
                let Some(pr_str) = parts.next() else { continue };
                let Some(remainder) = parts.next() else {
                    continue;
                };
                let Ok(pr_num) = pr_str.parse::<u64>() else {
                    return Err(BundleError::Invalid(format!(
                        "invalid PR number in path: prs/{pr_str}/..."
                    )));
                };
                if remainder == "meta.json" {
                    pr_meta.insert(pr_num, bytes);
                } else if let Some(spec_path) = remainder.strip_prefix("specs/") {
                    pr_files
                        .entry(pr_num)
                        .or_default()
                        .push((spec_path.to_string(), bytes));
                } else {
                    // Unknown payload under prs/<n>/; ignore for forward compat.
                }
                continue;
            }
            // Unknown top-level entry; ignore for forward compatibility.
        }

        let mainline = Mainline {
            specs: group_specs(mainline_files)?,
        };

        let mut pr_changes = Vec::new();
        for (pr_number, meta_bytes) in pr_meta.into_iter() {
            let meta: PrMetaWire = serde_json::from_slice(&meta_bytes)?;
            if meta.pr_number != pr_number {
                return Err(BundleError::Invalid(format!(
                    "PR meta number mismatch at prs/{pr_number}/meta.json: meta says {}",
                    meta.pr_number
                )));
            }

            let upserts = group_specs(pr_files.remove(&pr_number).unwrap_or_default())?;

            let mut spec_changes: Vec<SpecChange> =
                upserts.into_iter().map(SpecChange::Upsert).collect();
            for id in meta.removed_specs {
                spec_changes.push(SpecChange::Remove { id });
            }

            let asset_changes: Vec<AssetChange> = meta
                .removed_assets
                .into_iter()
                .map(|r| AssetChange::Remove {
                    spec_id: r.spec_id,
                    path: r.path,
                })
                .collect();

            pr_changes.push(PrChangeSet {
                pr_number,
                branch: meta.branch,
                head_sha: meta.head_sha,
                title: meta.title,
                author: meta.author,
                state: meta.state,
                url: meta.url,
                created_at: meta.created_at,
                updated_at: meta.updated_at,
                spec_changes,
                asset_changes,
                spec_meta: meta.spec_meta,
            });
        }

        // Any PR directories without a meta.json are reported so the caller
        // doesn't silently lose data.
        if let Some((pr, _)) = pr_files.into_iter().next() {
            return Err(BundleError::Invalid(format!(
                "prs/{pr}/specs/... present but prs/{pr}/meta.json missing"
            )));
        }

        Ok(Package {
            manifest,
            project_config,
            mainline,
            pr_changes,
        })
    }
}

fn write_spec<W: Write + Seek>(
    zw: &mut ZipWriter<W>,
    prefix: &str,
    spec: &Spec,
    opts: SimpleFileOptions,
) -> Result<(), BundleError> {
    let src_path = format!("{prefix}/{}/{}", spec.dir_name, spec.source_path);
    zw.start_file(src_path, opts)?;
    zw.write_all(&spec.source)?;
    for asset in &spec.assets {
        let path = format!("{prefix}/{}/{}", spec.dir_name, asset.path);
        zw.start_file(path, opts)?;
        zw.write_all(&asset.bytes)?;
    }
    Ok(())
}

fn take_file(files: &mut Vec<(String, Vec<u8>)>, name: &str) -> Option<Vec<u8>> {
    let idx = files.iter().position(|(n, _)| n == name)?;
    Some(files.remove(idx).1)
}

/// Group flat `<dir>/<file>` entries into `Spec` records.
fn group_specs(files: Vec<(String, Vec<u8>)>) -> Result<Vec<Spec>, BundleError> {
    // Preserve insertion order of first appearance of each dir.
    let mut dir_order: Vec<String> = Vec::new();
    let mut by_dir: BTreeMap<String, Vec<(String, Vec<u8>)>> = BTreeMap::new();

    for (path, bytes) in files {
        let mut parts = path.splitn(2, '/');
        let Some(dir) = parts.next() else { continue };
        let Some(rest) = parts.next() else {
            // File directly under specs/ with no subdir — skip.
            continue;
        };
        if !by_dir.contains_key(dir) {
            dir_order.push(dir.to_string());
        }
        by_dir
            .entry(dir.to_string())
            .or_default()
            .push((rest.to_string(), bytes));
    }

    let mut specs = Vec::with_capacity(dir_order.len());
    for dir_name in dir_order {
        let entries = by_dir.remove(&dir_name).unwrap_or_default();

        let id = extract_spec_id(&dir_name).ok_or_else(|| {
            BundleError::Invalid(format!(
                "spec directory {dir_name} does not start with a numeric prefix"
            ))
        })?;

        // Pick the source file: prefer Markdown, then AsciiDoc; among
        // candidates of equal precedence pick the alphabetically first.
        let mut markdown: Vec<&(String, Vec<u8>)> = entries
            .iter()
            .filter(|(p, _)| matches!(DocFormat::from_extension(p), Some(DocFormat::Markdown)))
            .collect();
        let mut asciidoc: Vec<&(String, Vec<u8>)> = entries
            .iter()
            .filter(|(p, _)| matches!(DocFormat::from_extension(p), Some(DocFormat::Asciidoc)))
            .collect();
        markdown.sort_by(|a, b| a.0.cmp(&b.0));
        asciidoc.sort_by(|a, b| a.0.cmp(&b.0));

        let (source_path, format, source_bytes) = if let Some((p, b)) = markdown.first() {
            (p.clone(), DocFormat::Markdown, b.clone())
        } else if let Some((p, b)) = asciidoc.first() {
            (p.clone(), DocFormat::Asciidoc, b.clone())
        } else {
            return Err(BundleError::Invalid(format!(
                "spec directory {dir_name} contains no Markdown or AsciiDoc source"
            )));
        };

        let assets: Vec<Asset> = entries
            .into_iter()
            .filter(|(p, _)| *p != source_path)
            .map(|(p, b)| Asset {
                path: p,
                content_type: None,
                bytes: b,
            })
            .collect();

        specs.push(Spec {
            id,
            dir_name,
            format,
            source_path,
            source: source_bytes,
            assets,
        });
    }

    Ok(specs)
}

/// A mainline spec on disk: either a subdirectory of files or a single flat
/// source file.
enum SpecSource {
    Dir(std::path::PathBuf),
    File {
        name: String,
        path: std::path::PathBuf,
    },
}

fn visit_spec_dir(
    dir: &Path,
    in_zip_prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), BundleError> {
    let mut subentries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        subentries.push((name, entry.path()));
    }
    subentries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in subentries {
        let zip_path = format!("{in_zip_prefix}/{name}");
        if path.is_dir() {
            visit_spec_dir(&path, &zip_path, out)?;
        } else if path.is_file() {
            let bytes = fs::read(&path)?;
            out.push((zip_path, bytes));
        }
    }
    Ok(())
}

/// Extract the spec id from a directory name like `"0001-authentication"`.
/// Requires at least four leading digits followed by `-`.
pub fn extract_spec_id(dir_name: &str) -> Option<String> {
    let digits: String = dir_name
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() < 4 {
        return None;
    }
    if dir_name.as_bytes().get(digits.len()) != Some(&b'-') {
        return None;
    }
    Some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_package() -> Package {
        Package {
            manifest: Manifest {
                package_version: PACKAGE_VERSION,
                commit: Some("abc123".into()),
                branch: Some("main".into()),
                timestamp: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
                source: SourceMode::Push,
                specs: Vec::new(),
                base_commit: None,
                deleted_specs: Vec::new(),
                deleted_prs: Vec::new(),
            },
            project_config: Some(b"# dossiers.toml\n".to_vec()),
            mainline: Mainline {
                specs: vec![
                    Spec {
                        id: "0001".into(),
                        dir_name: "0001-authentication".into(),
                        format: DocFormat::Markdown,
                        source_path: "authentication.md".into(),
                        source: b"# Authentication\n".to_vec(),
                        assets: vec![Asset {
                            path: "diagram.png".into(),
                            content_type: None,
                            bytes: vec![137, 80, 78, 71],
                        }],
                    },
                    Spec {
                        id: "0002".into(),
                        dir_name: "0002-api".into(),
                        format: DocFormat::Asciidoc,
                        source_path: "api.adoc".into(),
                        source: b"= API\n".to_vec(),
                        assets: vec![],
                    },
                ],
            },
            pr_changes: vec![PrChangeSet {
                pr_number: 42,
                branch: "feature/auth".into(),
                head_sha: "deadbeef".into(),
                title: "Rewrite authentication".into(),
                author: Some("octocat".into()),
                state: "REVIEW".into(),
                url: "https://github.com/acme/specs/pull/42".into(),
                created_at: DateTime::<Utc>::from_timestamp(1_700_000_100, 0),
                updated_at: DateTime::<Utc>::from_timestamp(1_700_000_200, 0),
                spec_meta: vec![PrSpecMeta {
                    spec_id: "0001".into(),
                    title: "Authentication (rewrite)".into(),
                    status: "REVIEW".into(),
                    authors: vec!["octocat".into()],
                    authors_meta: vec![Author {
                        name: "octocat".into(),
                        avatar_url: Some("https://avatars.githubusercontent.com/u/583231".into()),
                        url: Some("https://github.com/octocat".into()),
                    }],
                    created: DateTime::<Utc>::from_timestamp(1_700_000_100, 0),
                    updated: DateTime::<Utc>::from_timestamp(1_700_000_200, 0),
                    links: vec![],
                    fields: vec![],
                }],
                spec_changes: vec![
                    SpecChange::Upsert(Spec {
                        id: "0001".into(),
                        dir_name: "0001-authentication".into(),
                        format: DocFormat::Markdown,
                        source_path: "authentication.md".into(),
                        source: b"# Authentication (rewrite)\n".to_vec(),
                        assets: vec![],
                    }),
                    SpecChange::Remove { id: "0099".into() },
                ],
                asset_changes: vec![AssetChange::Remove {
                    spec_id: "0002".into(),
                    path: "old.svg".into(),
                }],
            }],
        }
    }

    #[test]
    fn roundtrip_preserves_content() {
        let pkg = sample_package();
        let mut buf = Cursor::new(Vec::new());
        pkg.write_zip(&mut buf).unwrap();
        buf.set_position(0);
        let decoded = Package::read_zip(buf).unwrap();

        assert_eq!(decoded.manifest.package_version, PACKAGE_VERSION);
        assert_eq!(decoded.manifest.commit.as_deref(), Some("abc123"));
        assert_eq!(decoded.manifest.branch.as_deref(), Some("main"));
        assert_eq!(decoded.manifest.source, SourceMode::Push);
        assert_eq!(
            decoded.project_config.as_deref(),
            Some(&b"# dossiers.toml\n"[..])
        );

        assert_eq!(decoded.mainline.specs.len(), 2);

        let auth = &decoded.mainline.specs[0];
        assert_eq!(auth.id, "0001");
        assert_eq!(auth.dir_name, "0001-authentication");
        assert_eq!(auth.format, DocFormat::Markdown);
        assert_eq!(auth.source_path, "authentication.md");
        assert_eq!(auth.source, b"# Authentication\n");
        assert_eq!(auth.assets.len(), 1);
        assert_eq!(auth.assets[0].path, "diagram.png");
        assert_eq!(auth.assets[0].bytes, vec![137, 80, 78, 71]);

        let api = &decoded.mainline.specs[1];
        assert_eq!(api.format, DocFormat::Asciidoc);
        assert_eq!(api.source_path, "api.adoc");
        assert!(api.assets.is_empty());

        assert_eq!(decoded.pr_changes.len(), 1);
        let pr = &decoded.pr_changes[0];
        assert_eq!(pr.pr_number, 42);
        assert_eq!(pr.branch, "feature/auth");
        assert_eq!(pr.head_sha, "deadbeef");
        assert_eq!(pr.title, "Rewrite authentication");
        assert_eq!(pr.author.as_deref(), Some("octocat"));
        assert_eq!(pr.state, "REVIEW");
        assert_eq!(pr.url, "https://github.com/acme/specs/pull/42");
        assert_eq!(
            pr.created_at,
            DateTime::<Utc>::from_timestamp(1_700_000_100, 0)
        );
        assert_eq!(pr.spec_meta.len(), 1);
        assert_eq!(pr.spec_meta[0].spec_id, "0001");
        assert_eq!(pr.spec_meta[0].title, "Authentication (rewrite)");
        assert_eq!(pr.spec_meta[0].authors, vec!["octocat".to_string()]);
        assert_eq!(
            pr.spec_meta[0].authors_meta,
            vec![Author {
                name: "octocat".into(),
                avatar_url: Some("https://avatars.githubusercontent.com/u/583231".into()),
                url: Some("https://github.com/octocat".into()),
            }]
        );

        let upserts: Vec<&Spec> = pr
            .spec_changes
            .iter()
            .filter_map(|c| match c {
                SpecChange::Upsert(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].source, b"# Authentication (rewrite)\n");

        let removes: Vec<&str> = pr
            .spec_changes
            .iter()
            .filter_map(|c| match c {
                SpecChange::Remove { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(removes, vec!["0099"]);

        let asset_removes: Vec<(&str, &str)> = pr
            .asset_changes
            .iter()
            .filter_map(|c| match c {
                AssetChange::Remove { spec_id, path } => Some((spec_id.as_str(), path.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(asset_removes, vec![("0002", "old.svg")]);
    }

    fn temp_specs_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dossiers-mainline-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp specs dir");
        dir
    }

    #[test]
    fn mainline_from_directory_reads_flat_files() {
        // The React RFC layout: flat `NNNN-name.md` files, no per-spec dirs.
        let dir = temp_specs_dir("flat");
        fs::write(dir.join("0002-context.md"), b"# Context\n").unwrap();
        fs::write(dir.join("0006-lifecycle.md"), b"# Lifecycle\n").unwrap();
        fs::write(dir.join("README.md"), b"# not a spec\n").unwrap();
        fs::write(dir.join(".hidden-0001-x.md"), b"# hidden\n").unwrap();

        let mainline = Package::mainline_from_directory(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(mainline.specs.len(), 2);
        let ctx = &mainline.specs[0];
        assert_eq!(ctx.id, "0002");
        assert_eq!(ctx.dir_name, "0002-context");
        assert_eq!(ctx.source_path, "0002-context.md");
        assert_eq!(ctx.format, DocFormat::Markdown);
        assert_eq!(ctx.source, b"# Context\n");
        assert!(ctx.assets.is_empty());
        assert_eq!(mainline.specs[1].id, "0006");
    }

    #[test]
    fn mainline_from_directory_mixes_files_and_dirs() {
        let dir = temp_specs_dir("mixed");
        fs::write(dir.join("0001-flat.md"), b"# Flat\n").unwrap();
        let sub = dir.join("0002-nested");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("spec.md"), b"# Nested\n").unwrap();
        fs::write(sub.join("diagram.png"), vec![137, 80, 78, 71]).unwrap();

        let mainline = Package::mainline_from_directory(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(mainline.specs.len(), 2);
        // Sorted by directory name: flat first, then nested.
        assert_eq!(mainline.specs[0].id, "0001");
        assert_eq!(mainline.specs[0].source_path, "0001-flat.md");
        assert!(mainline.specs[0].assets.is_empty());

        let nested = &mainline.specs[1];
        assert_eq!(nested.id, "0002");
        assert_eq!(nested.dir_name, "0002-nested");
        assert_eq!(nested.source_path, "spec.md");
        assert_eq!(nested.assets.len(), 1);
        assert_eq!(nested.assets[0].path, "diagram.png");
    }

    #[test]
    fn rejects_unknown_package_version() {
        let mut pkg = sample_package();
        pkg.manifest.package_version = 99;
        let mut buf = Cursor::new(Vec::new());
        pkg.write_zip(&mut buf).unwrap();
        buf.set_position(0);
        let err = Package::read_zip(buf).unwrap_err();
        assert!(matches!(err, BundleError::Invalid(_)));
    }

    #[test]
    fn extract_spec_id_requires_four_digits_and_dash() {
        assert_eq!(extract_spec_id("0001-x"), Some("0001".into()));
        assert_eq!(extract_spec_id("12345-x"), Some("12345".into()));
        assert_eq!(extract_spec_id("001-x"), None);
        assert_eq!(extract_spec_id("0001x"), None);
        assert_eq!(extract_spec_id("abcd-x"), None);
    }
}
