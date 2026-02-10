use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod metadata;

use actix_files::Files;
use actix_web::{rt::task, web, App, HttpResponse, HttpServer, Responder};
use anyhow::{anyhow, bail, Context, Result};
use asciidoc_parser::{
    attributes::Attrlist,
    blocks::{
        Block as AsciidocBlock, Break as AsciidocBreak, BreakType, CompoundDelimitedBlock,
        IsBlock as _, MediaBlock, MediaType, RawDelimitedBlock, SectionBlock, SimpleBlock,
        SimpleBlockStyle,
    },
    document::Document as AsciidocDocument,
    Parser as AsciidocParser,
};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use dossiers::git_utils::{open_git_repository, GitTimestampCache};
use dossiers::github::{parse_github_repo, GithubClient, GithubFile, GithubPull};
use lazy_static::lazy_static;
use maud::{html, Markup, PreEscaped};
use metadata::{
    ExtraMetadataField, MetadataReader, MetadataValue, MetadataValueType, ProjectConfiguration,
};
use pulldown_cmark::{
    html as md_html, CodeBlockKind, Event, Options as MdOptions, Parser, Tag, TagEnd,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use thiserror::Error;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

const EMBEDDED_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/global.css"));
const EMBEDDED_FAVICON: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/favicon.svg"));
const THEME_INIT_SCRIPT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/theme-init.js"));
const THEME_TOGGLE_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/theme-toggle.js"
));
const MINI_TOC_SCRIPT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/mini-toc.js"));
const INDEX_SEARCH_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/index-search.js"
));
const MERMAID_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/mermaid.min.js"
));
const MERMAID_INIT_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/mermaid-init.js"
));
const CREATE_MODAL_JS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/create-modal.js"
));

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeneratedSpec {
    id: String,
    dir_name: String,
    title: String,
    status: String,
    #[serde(default)]
    created: Option<Value>,
    #[serde(default)]
    updated: Option<Value>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    links: Vec<Link>,
    #[serde(default)]
    updated_sort: Option<Value>,
    #[serde(default)]
    extra: HashMap<String, Value>,
    source: String,
    format: String,
}

#[derive(Debug, Clone, Copy)]
enum DocFormat {
    Asciidoc,
    Markdown,
}

#[derive(Debug, Clone)]
struct SpecDocument {
    id: String,
    dir_name: String,
    title: String,
    status: String,
    created: Option<i64>,
    updated: Option<i64>,
    authors: Vec<String>,
    links: Vec<Link>,
    updated_sort: i64,
    extra: HashMap<String, Value>,
    source: String,
    format: DocFormat,
    listed: bool,
    revision_of: Option<String>,
    pr_number: Option<u64>,
}

#[derive(Debug, Clone)]
struct StatusSummary {
    name: String,
    slug: String,
    count: usize,
}

#[derive(Debug)]
struct PendingSpec {
    id: String,
    dir_name: String,
    title: String,
    status: Option<String>,
    authors: Vec<String>,
    links: Vec<Link>,
    extra: HashMap<String, Value>,
    body: String,
    format: DocFormat,
    meta_created: Option<i64>,
    meta_updated: Option<i64>,
    git_paths: Vec<PathBuf>,
    doc_path: PathBuf,
}

#[derive(Debug)]
struct LintSpec {
    id: String,
    dir_name: String,
    doc_path: PathBuf,
    spec_root: PathBuf,
    source: String,
    format: DocFormat,
}

#[derive(Debug)]
struct AssetReference {
    path: String,
    line: usize,
}

#[derive(Debug)]
struct CrossReference {
    raw: String,
    target_spec: String,
    anchor: Option<String>,
    line: usize,
}

#[derive(Debug, Default)]
struct SpecLintData {
    anchors: Vec<String>,
    assets: Vec<AssetReference>,
    cross_refs: Vec<CrossReference>,
}

#[derive(Debug)]
struct LintMessage {
    message: String,
    location: Option<String>,
}

#[derive(Debug, Default)]
struct CategoryReport {
    errors: Vec<LintMessage>,
    warnings: Vec<LintMessage>,
}

impl CategoryReport {
    fn add_error(&mut self, message: impl Into<String>, location: Option<String>) {
        self.errors.push(LintMessage {
            message: message.into(),
            location,
        });
    }

    fn add_warning(&mut self, message: impl Into<String>, location: Option<String>) {
        self.warnings.push(LintMessage {
            message: message.into(),
            location,
        });
    }
}

#[derive(Clone)]
struct Assets {
    css_source: CssSource,
    favicon_source: FaviconSource,
    theme_init_source: ScriptSource,
    theme_toggle_source: ScriptSource,
    mini_toc_source: ScriptSource,
    index_search_source: ScriptSource,
    mermaid_source: ScriptSource,
    mermaid_init_source: ScriptSource,
}

#[derive(Clone)]
enum CssSource {
    Embedded(&'static str),
    File(PathBuf),
}

#[derive(Clone)]
enum FaviconSource {
    Embedded(&'static [u8]),
    File(PathBuf),
}

#[derive(Clone)]
enum ScriptSource {
    Embedded(&'static str),
    File(PathBuf),
}

impl Assets {
    fn embedded() -> Self {
        Self {
            css_source: CssSource::Embedded(EMBEDDED_CSS),
            favicon_source: FaviconSource::Embedded(EMBEDDED_FAVICON),
            theme_init_source: ScriptSource::Embedded(THEME_INIT_SCRIPT),
            theme_toggle_source: ScriptSource::Embedded(THEME_TOGGLE_SCRIPT),
            mini_toc_source: ScriptSource::Embedded(MINI_TOC_SCRIPT),
            index_search_source: ScriptSource::Embedded(INDEX_SEARCH_SCRIPT),
            mermaid_source: ScriptSource::Embedded(MERMAID_SCRIPT),
            mermaid_init_source: ScriptSource::Embedded(MERMAID_INIT_SCRIPT),
        }
    }

    fn from_assets_dir(dir: PathBuf) -> Self {
        let css_path = dir.join("global.css");
        let favicon_path = dir.join("favicon.svg");
        let theme_init_path = dir.join("theme-init.js");
        let theme_toggle_path = dir.join("theme-toggle.js");
        let mini_toc_path = dir.join("mini-toc.js");
        let index_search_path = dir.join("index-search.js");
        let mermaid_path = dir.join("mermaid.min.js");
        let mermaid_init_path = dir.join("mermaid-init.js");

        let css_source = if css_path.exists() {
            CssSource::File(css_path)
        } else {
            CssSource::Embedded(EMBEDDED_CSS)
        };

        let favicon_source = if favicon_path.exists() {
            FaviconSource::File(favicon_path)
        } else {
            FaviconSource::Embedded(EMBEDDED_FAVICON)
        };

        let theme_init_source = if theme_init_path.exists() {
            ScriptSource::File(theme_init_path)
        } else {
            ScriptSource::Embedded(THEME_INIT_SCRIPT)
        };

        let theme_toggle_source = if theme_toggle_path.exists() {
            ScriptSource::File(theme_toggle_path)
        } else {
            ScriptSource::Embedded(THEME_TOGGLE_SCRIPT)
        };

        let mini_toc_source = if mini_toc_path.exists() {
            ScriptSource::File(mini_toc_path)
        } else {
            ScriptSource::Embedded(MINI_TOC_SCRIPT)
        };

        let index_search_source = if index_search_path.exists() {
            ScriptSource::File(index_search_path)
        } else {
            ScriptSource::Embedded(INDEX_SEARCH_SCRIPT)
        };

        let mermaid_source = if mermaid_path.exists() {
            ScriptSource::File(mermaid_path)
        } else {
            ScriptSource::Embedded(MERMAID_SCRIPT)
        };

        let mermaid_init_source = if mermaid_init_path.exists() {
            ScriptSource::File(mermaid_init_path)
        } else {
            ScriptSource::Embedded(MERMAID_INIT_SCRIPT)
        };

        Self {
            css_source,
            favicon_source,
            theme_init_source,
            theme_toggle_source,
            mini_toc_source,
            index_search_source,
            mermaid_source,
            mermaid_init_source,
        }
    }

    fn css(&self) -> String {
        match &self.css_source {
            CssSource::Embedded(css) => css.to_string(),
            CssSource::File(path) => match fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(err) => {
                    eprintln!(
                        "Warning: failed to read CSS at {}: {err}. Falling back to embedded CSS.",
                        path.display()
                    );
                    EMBEDDED_CSS.to_string()
                }
            },
        }
    }

    fn favicon(&self) -> Vec<u8> {
        match &self.favicon_source {
            FaviconSource::Embedded(bytes) => bytes.to_vec(),
            FaviconSource::File(path) => match fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!(
                        "Warning: failed to read favicon at {}: {err}. Falling back to embedded favicon.",
                        path.display()
                    );
                    EMBEDDED_FAVICON.to_vec()
                }
            },
        }
    }

    fn read_script(source: &ScriptSource, fallback: &'static str, label: &str) -> String {
        match source {
            ScriptSource::Embedded(js) => js.to_string(),
            ScriptSource::File(path) => match fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(err) => {
                    eprintln!(
                        "Warning: failed to read {label} script at {}: {err}. Falling back to embedded version.",
                        path.display()
                    );
                    fallback.to_string()
                }
            },
        }
    }

    fn theme_init_script(&self) -> String {
        Self::read_script(&self.theme_init_source, THEME_INIT_SCRIPT, "theme init")
    }

    fn theme_toggle_script(&self) -> String {
        Self::read_script(
            &self.theme_toggle_source,
            THEME_TOGGLE_SCRIPT,
            "theme toggle",
        )
    }

    fn mini_toc_script(&self) -> String {
        Self::read_script(&self.mini_toc_source, MINI_TOC_SCRIPT, "mini TOC")
    }

    fn index_search_script(&self) -> String {
        Self::read_script(
            &self.index_search_source,
            INDEX_SEARCH_SCRIPT,
            "index search",
        )
    }

    fn mermaid_script(&self) -> String {
        Self::read_script(&self.mermaid_source, MERMAID_SCRIPT, "mermaid runtime")
    }

    fn mermaid_init_script(&self) -> String {
        Self::read_script(
            &self.mermaid_init_source,
            MERMAID_INIT_SCRIPT,
            "mermaid init",
        )
    }
}

#[derive(Clone)]
struct AppState {
    specs: Vec<SpecDocument>,
    specs_by_id: HashMap<String, SpecDocument>,
    spec_ids: HashSet<String>,
    revisions: HashMap<String, Vec<RevisionLink>>,
    display_prefix: String,
    site_name: String,
    site_description: String,
    github_repo: Option<String>,
    extra_fields: Vec<ExtraMetadataField>,
    generated_at: i64,
    assets: Assets,
    renderer: DocRenderer,
    create_config: Option<CreateDocConfig>,
}

#[derive(Clone)]
struct CreateDocConfig {
    github_repo: String,
    default_branch: String,
    subdirectory: Option<String>,
    next_id: u64,
    format: String,    // "md" or "adoc"
    structure: String, // "directory" or "flat"
}

type StaticMount = (String, PathBuf);

#[derive(Clone)]
struct RevisionLink {
    pr_number: u64,
    status: String,
    updated: Option<i64>,
    href: String,
}

struct LoadResult {
    specs: Vec<SpecDocument>,
    static_mounts: Vec<StaticMount>,
}

#[derive(Clone)]
struct ReloadableAppState {
    input_path: PathBuf,
    project_root: PathBuf,
    config_path: Option<PathBuf>,
    assets: Assets,
}

impl ReloadableAppState {
    fn load(&self) -> Result<AppState> {
        let project_config =
            load_project_configuration(&self.project_root, self.config_path.as_deref());
        let site_name = resolve_site_name(&self.project_root, &project_config);
        build_app_state(
            &self.input_path,
            &self.project_root,
            site_name,
            self.assets.clone(),
            project_config,
        )
        .map(|(state, _)| state)
    }

    fn assets(&self) -> &Assets {
        &self.assets
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct ParsedMetadata {
    title: String,
    status: String,
    created: Option<i64>,
    updated: Option<i64>,
    authors: Vec<String>,
    links: Vec<Link>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct ParsedDoc {
    metadata: ParsedMetadata,
    body: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
struct Link {
    label: String,
    href: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FrontmatterAuthors {
    Single(String),
    List(Vec<String>),
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Frontmatter {
    title: Option<String>,
    status: Option<String>,
    created: Option<String>,
    updated: Option<String>,
    authors: Option<FrontmatterAuthors>,
    links: Option<HashMap<String, String>>,
}

impl FrontmatterAuthors {
    #[allow(dead_code)]
    fn into_vec(self) -> Vec<String> {
        match self {
            FrontmatterAuthors::Single(value) => vec![value],
            FrontmatterAuthors::List(values) => values,
        }
    }
}

#[allow(dead_code)]
fn parse_doc_metadata(source: &str, format: &DocFormat, fallback_title: &str) -> ParsedMetadata {
    let mut status = "DRAFT".to_string();
    let mut created = None;
    let mut updated = None;
    let mut authors = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(':') {
            continue;
        }
        let rest = &trimmed[1..];
        let Some((key, raw_value)) = rest.split_once(':') else {
            continue;
        };
        let value = raw_value.trim();
        match key.to_lowercase().as_str() {
            "status" => {
                if !value.is_empty() {
                    status = value.to_string();
                }
            }
            "created" => {
                created = parse_date(value);
            }
            "updated" | "date" => {
                updated = parse_date(value);
            }
            "author" | "authors" => {
                if !value.is_empty() {
                    authors.extend(
                        value
                            .split([',', ';'])
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string()),
                    );
                }
            }
            _ => {}
        }
    }

    let title = extract_leading_title(source, format).unwrap_or_else(|| fallback_title.to_string());

    ParsedMetadata {
        title,
        status,
        created,
        updated,
        authors: normalize_authors(authors),
        links: Vec::new(),
    }
}

fn parse_markdown_frontmatter(source: &str) -> Option<(Frontmatter, String)> {
    let mut lines = source.split_inclusive('\n');
    let first_line = lines.next()?;
    if first_line.trim() != "---" {
        return None;
    }

    let mut frontmatter_block = String::new();
    let mut consumed = first_line.len();

    for line in lines {
        consumed += line.len();
        if line.trim() == "---" {
            let frontmatter: Frontmatter = parse_frontmatter_block(&frontmatter_block);
            let body = source.get(consumed..).unwrap_or("").to_string();
            return Some((frontmatter, body));
        }
        frontmatter_block.push_str(line);
    }

    None
}

fn parse_frontmatter_block(block: &str) -> Frontmatter {
    serde_yaml::from_str(block).unwrap_or_else(|_| {
        let cleaned = sanitize_frontmatter_block(block);
        serde_yaml::from_str(&cleaned).unwrap_or_default()
    })
}

fn sanitize_frontmatter_block(block: &str) -> String {
    lazy_static! {
        static ref BARE_DASH_VALUE: Regex = Regex::new(r"^(\s*[^:#]+:\s*)-\s*$").unwrap();
    }

    block
        .lines()
        .map(|line| {
            if let Some(prefix) = BARE_DASH_VALUE
                .captures(line)
                .and_then(|caps| caps.get(1).map(|m| m.as_str()))
            {
                format!("{prefix}\"\"")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_frontmatter(source: &str, format: &DocFormat) -> String {
    match format {
        DocFormat::Markdown => parse_markdown_frontmatter(source)
            .map(|(_, body)| body)
            .unwrap_or_else(|| source.to_string()),
        DocFormat::Asciidoc => source.to_string(),
    }
}

#[allow(dead_code)]
fn parse_doc(source: &str, format: &DocFormat, fallback_title: &str) -> ParsedDoc {
    if let DocFormat::Markdown = format {
        if let Some((frontmatter, body)) = parse_markdown_frontmatter(source) {
            let links = frontmatter
                .links
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(label, href)| {
                    let trimmed_href = href.trim();
                    let trimmed_label = label.trim();
                    if trimmed_href.is_empty() || trimmed_label.is_empty() {
                        None
                    } else {
                        Some(Link {
                            label: trimmed_label.to_string(),
                            href: trimmed_href.to_string(),
                        })
                    }
                })
                .collect::<Vec<_>>();

            let leading_title = extract_leading_title(&body, format);
            let frontmatter_title = frontmatter
                .title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty());

            let metadata = ParsedMetadata {
                title: leading_title
                    .or(frontmatter_title)
                    .unwrap_or_else(|| fallback_title.to_string()),
                status: frontmatter.status.unwrap_or_else(|| "DRAFT".to_string()),
                created: frontmatter.created.as_deref().and_then(parse_date),
                updated: frontmatter.updated.as_deref().and_then(parse_date),
                authors: normalize_authors(
                    frontmatter
                        .authors
                        .map(FrontmatterAuthors::into_vec)
                        .unwrap_or_default(),
                ),
                links,
            };
            return ParsedDoc { metadata, body };
        }
    }

    let metadata = parse_doc_metadata(source, format, fallback_title);
    ParsedDoc {
        metadata,
        body: source.to_string(),
    }
}

fn normalize_authors<I>(authors: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    lazy_static! {
        static ref EMAIL_RE: Regex = Regex::new(r"(?i)\s*<[^>]+>").unwrap();
    }

    authors
        .into_iter()
        .filter_map(|author| {
            let cleaned = EMAIL_RE.replace_all(author.trim(), "").trim().to_string();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

fn metadata_extra_to_json(map: &HashMap<String, MetadataValue>) -> HashMap<String, Value> {
    map.iter()
        .filter_map(|(k, v)| metadata_value_to_json(v).map(|vv| (k.clone(), vv)))
        .collect()
}

fn metadata_value_to_json(value: &MetadataValue) -> Option<Value> {
    match value {
        MetadataValue::String(s) => Some(Value::String(s.clone())),
        MetadataValue::Number(n) => Number::from_f64(*n).map(Value::Number),
        MetadataValue::Boolean(b) => Some(Value::Bool(*b)),
        MetadataValue::Markdown(html) => Some(Value::String(html.clone())),
    }
}

fn display_extra_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn url_escape_component(raw: &str) -> String {
    const UNRESERVED: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut encoded = String::new();
    for ch in raw.chars() {
        if UNRESERVED.contains(ch) {
            encoded.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

fn load_specs_from_json(path: &Path, _config: &ProjectConfiguration) -> Result<LoadResult> {
    let raw_specs: Vec<GeneratedSpec> = serde_json::from_reader(
        File::open(path).with_context(|| format!("Opening {}", path.display()))?,
    )
    .with_context(|| format!("Parsing {}", path.display()))?;

    let mut specs = Vec::with_capacity(raw_specs.len());
    for spec in raw_specs {
        let parsed = spec_from_generated(spec)?;
        specs.push(parsed);
    }

    let static_mounts = Vec::new();

    Ok(LoadResult {
        specs,
        static_mounts,
    })
}

fn load_specs_from_directory(
    dir: &Path,
    project_config: &ProjectConfiguration,
) -> Result<LoadResult> {
    if !dir.is_dir() {
        bail!("Provided path is not a directory: {}", dir.display());
    }

    let mut dir_locations: HashMap<String, (String, PathBuf)> = HashMap::new();
    let mut file_locations: HashMap<String, (String, PathBuf, DocFormat)> = HashMap::new();
    let mut ordered_ids = Vec::new();
    let mut discovered_ids: HashSet<String> = HashSet::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid entry name under {}", dir.display()))?
            .to_string();
        let Some(id) = extract_spec_id(&name) else {
            continue;
        };

        if discovered_ids.insert(id.clone()) {
            ordered_ids.push(id.clone());
        }

        if path.is_dir() {
            dir_locations.entry(id).or_insert((name, path));
            continue;
        }

        if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            let format = match ext.as_str() {
                "md" | "markdown" => Some(DocFormat::Markdown),
                "adoc" | "asciidoc" => Some(DocFormat::Asciidoc),
                _ => None,
            };

            if let Some(format) = format {
                let dir_name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(&name)
                    .to_string();
                file_locations.entry(id).or_insert((dir_name, path, format));
            }
        }
    }

    if dir_locations.is_empty() && file_locations.is_empty() {
        bail!(
            "No spec documents found in {} (expected subdirectories like 0001-* or files like 0001-*.md)",
            dir.display()
        );
    }

    let mut specs = Vec::new();
    let mut pending_specs: Vec<PendingSpec> = Vec::new();
    let mut static_mounts = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let metadata_reader = MetadataReader::new(project_config.clone());
    let git_repo = open_git_repository(dir);
    let mut all_git_paths: HashSet<PathBuf> = HashSet::new();

    for spec_id in ordered_ids {
        if seen_ids.contains(&spec_id) {
            continue;
        }
        let file_entry = file_locations.get(&spec_id);
        let dir_entry = dir_locations.get(&spec_id);

        let (dir_name, doc_path, format, static_root) =
            if let Some((dir_name, path, format)) = file_entry {
                let static_root = dir_entry
                    .map(|(_, path)| path.clone())
                    .or_else(|| path.parent().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| dir.to_path_buf());
                (dir_name.clone(), path.clone(), *format, static_root)
            } else if let Some((dir_name, path)) = dir_entry {
                let (doc_path, format) = find_doc_file(path)?;
                (dir_name.clone(), doc_path, format, path.clone())
            } else {
                continue;
            };
        seen_ids.insert(spec_id.clone());
        let source = fs::read_to_string(&doc_path)
            .with_context(|| format!("Reading spec document at {}", doc_path.display()))?;

        let display_name = display_name_from_dir(&dir_name);
        let parsed_doc = metadata_reader.read(&source, format, &display_name);
        let meta = parsed_doc.metadata;
        let title = meta
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| display_name.clone());

        let git_paths = git_repo.as_ref().map(|repo| {
            collect_spec_git_paths(&spec_id, &doc_path, &static_root, &source, format)
                .into_iter()
                .filter_map(|path| {
                    path.strip_prefix(repo.workdir())
                        .map(|p| p.to_path_buf())
                        .ok()
                })
                .collect::<Vec<_>>()
        });

        if let Some(paths) = git_paths.as_ref() {
            all_git_paths.extend(paths.iter().cloned());
        }

        pending_specs.push(PendingSpec {
            id: spec_id.clone(),
            dir_name,
            title,
            status: meta.status,
            authors: meta.authors,
            links: meta.links,
            extra: metadata_extra_to_json(&meta.extra),
            body: parsed_doc.body,
            format,
            meta_created: meta.created.as_deref().and_then(parse_date),
            meta_updated: meta.updated.as_deref().and_then(parse_date),
            git_paths: git_paths.unwrap_or_default(),
            doc_path: doc_path.clone(),
        });

        static_mounts.push((format!("/{}", spec_id), static_root));
    }

    let git_cache = if let Some(repo) = git_repo.as_ref() {
        if all_git_paths.is_empty() {
            None
        } else {
            Some(GitTimestampCache::from_paths(
                repo,
                &all_git_paths.iter().cloned().collect::<Vec<_>>(),
            ))
        }
    } else {
        None
    };

    for pending in pending_specs {
        let (git_addition, git_change) = git_cache
            .as_ref()
            .map(|cache| {
                (
                    cache.latest_addition(&pending.git_paths),
                    cache.latest_change(&pending.git_paths),
                )
            })
            .unwrap_or((None, None));

        let (file_created, file_modified) = file_timestamps(&pending.doc_path);

        let created = pending
            .meta_created
            .or(git_addition)
            .or(file_created)
            .or(file_modified);

        let updated = pending
            .meta_updated
            .or(git_change)
            .or(file_modified)
            .or(created);

        let updated_sort = updated
            .or(created)
            .unwrap_or_else(|| Utc::now().timestamp_millis());

        let git_managed = git_repo.is_some() && (git_addition.is_some() || git_change.is_some());
        let status = metadata_reader.resolve_status(pending.status.clone(), git_managed);

        specs.push(SpecDocument {
            id: pending.id,
            dir_name: pending.dir_name,
            title: pending.title,
            status,
            created,
            updated,
            authors: pending.authors,
            links: pending.links,
            updated_sort,
            extra: pending.extra,
            source: pending.body,
            format: pending.format,
            listed: true,
            revision_of: None,
            pr_number: None,
        });
    }

    Ok(LoadResult {
        specs,
        static_mounts,
    })
}

fn collect_spec_git_paths(
    spec_id: &str,
    doc_path: &Path,
    static_root: &Path,
    source: &str,
    format: DocFormat,
) -> Vec<PathBuf> {
    let doc = doc_path
        .canonicalize()
        .ok()
        .unwrap_or_else(|| doc_path.to_path_buf());
    let root = static_root
        .canonicalize()
        .ok()
        .unwrap_or_else(|| static_root.to_path_buf());

    let mut paths: HashSet<PathBuf> = HashSet::new();
    paths.insert(doc);

    if let Ok(rendered) = DocRenderer::new().render(source, format) {
        for asset in collect_doc_assets(&rendered, Some(spec_id)) {
            let asset_path = root.join(&asset);
            let resolved = asset_path
                .canonicalize()
                .ok()
                .unwrap_or_else(|| asset_path.to_path_buf());
            if resolved.exists() {
                paths.insert(resolved);
            }
        }
    }

    paths.into_iter().collect()
}

fn load_specs(input_path: &Path, project_config: &ProjectConfiguration) -> Result<LoadResult> {
    if input_path.is_dir() {
        load_specs_from_directory(input_path, project_config)
    } else {
        load_specs_from_json(input_path, project_config)
    }
}

fn resolve_spec_input_path(input_path: &Path, project_config: &ProjectConfiguration) -> PathBuf {
    if !input_path.is_dir() {
        return input_path.to_path_buf();
    }

    let Some(subdir) = project_config.subdirectory.as_ref() else {
        return input_path.to_path_buf();
    };

    let subdir_path = PathBuf::from(subdir);
    let candidate = if subdir_path.is_absolute() {
        subdir_path
    } else {
        input_path.join(subdir_path)
    };

    if candidate.exists() {
        candidate
    } else {
        eprintln!(
            "Warning: configured subdirectory '{}' not found under {}",
            subdir,
            input_path.display()
        );
        input_path.to_path_buf()
    }
}

fn load_and_sort_specs(
    input_path: &Path,
    project_config: &ProjectConfiguration,
) -> Result<(Vec<SpecDocument>, Vec<StaticMount>)> {
    let input_root = resolve_spec_input_path(input_path, project_config);
    let mut load_result = load_specs(&input_root, project_config)?;
    load_result.specs.sort_by(|a, b| {
        b.updated_sort
            .cmp(&a.updated_sort)
            .then_with(|| b.id.cmp(&a.id))
    });

    Ok((load_result.specs, load_result.static_mounts))
}

fn insert_spec_document(state: &mut AppState, spec: SpecDocument) {
    state.spec_ids.insert(spec.id.clone());
    state.specs_by_id.insert(spec.id.clone(), spec.clone());
    state.specs.push(spec);
}

fn spec_document_to_generated_spec(spec: SpecDocument) -> GeneratedSpec {
    GeneratedSpec {
        id: spec.id,
        dir_name: spec.dir_name,
        title: spec.title,
        status: spec.status,
        created: spec.created.map(|value| Value::Number(Number::from(value))),
        updated: spec.updated.map(|value| Value::Number(Number::from(value))),
        authors: spec.authors,
        links: spec.links,
        updated_sort: Some(Value::Number(Number::from(spec.updated_sort))),
        extra: spec.extra,
        source: spec.source,
        format: match spec.format {
            DocFormat::Markdown => "markdown".to_string(),
            DocFormat::Asciidoc => "asciidoc".to_string(),
        },
    }
}

#[derive(Debug, Error)]
enum RenderError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid UTF-8 from renderer: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[allow(dead_code)]
    #[error("Renderer failed: {0}")]
    Renderer(String),
}

#[derive(Debug)]
enum CliCommand {
    Serve(PathBuf),
    Prepare(PathBuf),
    Build {
        input_path: PathBuf,
        output_dir: PathBuf,
        trailing_slashes: bool,
    },
    Check(PathBuf),
    List(PathBuf),
}

#[actix_web::main]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let project_dir = project_dir_from_args(&raw_args);
    print_banner(project_dir.as_deref());

    let (config_path, command) = match parse_args(&raw_args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(err) = run_command(command, config_path).await {
        eprintln!("{err}");
        std::process::exit(1);
    }

    Ok(())
}

async fn run_command(command: CliCommand, config_path: Option<PathBuf>) -> Result<()> {
    match command {
        CliCommand::Serve(input_path) => run_server(input_path, config_path).await,
        CliCommand::Prepare(input_path) => {
            run_prepare(input_path, config_path)?;
            Ok(())
        }
        CliCommand::Build {
            input_path,
            output_dir,
            trailing_slashes,
        } => {
            task::spawn_blocking(move || {
                run_build(input_path, output_dir, config_path, trailing_slashes)
            })
            .await
            .map_err(|err| anyhow!("build task failed: {err}"))??;
            Ok(())
        }
        CliCommand::Check(input_path) => run_check(input_path, config_path),
        CliCommand::List(input_path) => run_list(input_path, config_path),
    }
}

fn print_banner(project_dir: Option<&Path>) {
    let version = env!("CARGO_PKG_VERSION");

    for line in banner_lines(version, project_dir) {
        eprintln!("{line}");
    }
    eprintln!();
}

fn banner_lines(version: &str, project_dir: Option<&Path>) -> Vec<String> {
    const REV_START: &str = "\u{001b}[7m";
    const REV_END: &str = "\u{001b}[0m";
    const BOLD_START: &str = "\u{001b}[1m";
    const BOLD_END: &str = "\u{001b}[22m";

    let use_color = supports_color();
    let fg = if use_color {
        "\u{001b}[38;2;120;170;255m"
    } else {
        ""
    };
    let reset = if use_color { "\u{001b}[0m" } else { "" };

    let art = [
        String::new(),
        format!("{fg}  ████▄{reset}"),
        format!("{fg}  █{REV_START}≣≣≣{REV_END}{fg}█{reset}"),
        format!("{fg}  █{REV_START}≣≣≣{REV_END}{fg}█{reset}"),
        format!("{fg}  █████{reset}"),
    ];

    art.into_iter()
        .enumerate()
        .map(|(idx, piece)| {
            let mut line = piece;
            line.push_str("  ");
            match idx {
                2 => line.push_str(&format!("{BOLD_START}Dossiers v{version}{BOLD_END}")),
                3 => line.push_str(&format_project_dir(project_dir)),
                _ => {}
            }
            line
        })
        .collect()
}

fn format_project_dir(project_dir: Option<&Path>) -> String {
    let Some(project_dir) = project_dir else {
        return "https://dossie.rs".to_string();
    };

    if let Ok(home) = env::var("HOME") {
        let home_path = Path::new(&home);
        if let Ok(stripped) = project_dir.strip_prefix(home_path) {
            if stripped.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", stripped.display());
        }
    }

    project_dir.display().to_string()
}

fn supports_color() -> bool {
    if env::var("NO_COLOR").is_ok() {
        return false;
    }
    if env::var("FORCE_COLOR").is_ok() {
        return true;
    }
    env::var("COLORTERM").is_ok() || env::var("TERM").map(|t| t != "dumb").unwrap_or(false)
}

fn project_dir_from_args(args: &[String]) -> Option<PathBuf> {
    let mut args = args.iter().peekable();
    while let Some(flag) = args.peek() {
        match flag.as_str() {
            "-c" | "--config" => {
                args.next();
                let _ = args.next();
            }
            _ => break,
        }
    }

    let command = args.next()?;

    let raw_path = match command.as_str() {
        "serve" | "prepare" => args.next().map(PathBuf::from),
        "build" => {
            let mut input_path = None;

            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-o" | "--output" => {
                        // Skip the value for --output if present.
                        let _ = args.next();
                    }
                    "--trailing-slashes" => {}
                    _ if input_path.is_none() => input_path = Some(PathBuf::from(arg)),
                    _ => {}
                }
            }

            input_path
        }
        _ => None,
    }?;

    Some(absolutize_project_dir(raw_path))
}

fn absolutize_project_dir(path: PathBuf) -> PathBuf {
    let path = if path.is_absolute() {
        path
    } else if let Ok(cwd) = env::current_dir() {
        cwd.join(path)
    } else {
        path
    };

    fs::canonicalize(&path).unwrap_or(path)
}

fn parse_args(args: &[String]) -> Result<(Option<PathBuf>, CliCommand)> {
    let mut iter = args.iter().peekable();
    let mut config_path: Option<PathBuf> = None;

    while let Some(flag) = iter.peek() {
        match flag.as_str() {
            "-c" | "--config" => {
                iter.next();
                let Some(path) = iter.next() else {
                    bail!("Missing value for --config");
                };
                config_path = Some(PathBuf::from(path));
            }
            _ => break,
        }
    }

    let remaining: Vec<String> = iter.cloned().collect();
    let command = parse_command(&remaining)?;
    Ok((config_path, command))
}

fn parse_command(args: &[String]) -> Result<CliCommand> {
    let mut args = args.iter();
    let Some(command) = args.next() else {
        bail!("Missing command");
    };

    match command.as_str() {
        "serve" => {
            let path = args.next().cloned().unwrap_or_else(|| ".".to_string());
            if args.next().is_some() {
                bail!("Unexpected argument for serve");
            }
            Ok(CliCommand::Serve(validate_path(path)?))
        }
        "prepare" => {
            let path = args.next().cloned().unwrap_or_else(|| ".".to_string());
            if args.next().is_some() {
                bail!("Unexpected argument for prepare");
            }
            Ok(CliCommand::Prepare(validate_path(path)?))
        }
        "build" => {
            let mut input_path = None;
            let mut output_dir = None;
            let mut trailing_slashes = false;

            let mut args = args.cloned();

            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-o" | "--output" => {
                        let path = args
                            .next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for --output"))?;
                        output_dir = Some(PathBuf::from(path));
                    }
                    "--trailing-slashes" => {
                        trailing_slashes = true;
                    }
                    _ if input_path.is_none() => {
                        input_path = Some(arg);
                    }
                    _ => bail!("Unexpected argument for build: {arg}"),
                }
            }

            let input = validate_path(input_path.unwrap_or_else(|| ".".to_string()))?;
            let output = output_dir.unwrap_or_else(|| PathBuf::from("output"));
            Ok(CliCommand::Build {
                input_path: input,
                output_dir: output,
                trailing_slashes,
            })
        }
        "list" => {
            let path = args.next().cloned().unwrap_or_else(|| ".".to_string());
            if args.next().is_some() {
                bail!("Unexpected argument for list");
            }
            Ok(CliCommand::List(validate_path(path)?))
        }
        "check" => {
            let path = args.next().cloned().unwrap_or_else(|| ".".to_string());
            if args.next().is_some() {
                bail!("Unexpected argument for check");
            }
            Ok(CliCommand::Check(validate_path(path)?))
        }
        _ => bail!("Unknown command: {command}"),
    }
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  dossiers [-c <config-file>] serve [<path-to-spec-directory>]");
    eprintln!("  dossiers [-c <config-file>] prepare [<path-to-spec-directory>]");
    eprintln!("  dossiers [-c <config-file>] build [<path-to-spec-directory>] [-o <output-dir>] [--trailing-slashes]");
    eprintln!("  dossiers [-c <config-file>] list [<path-to-spec-directory>]");
    eprintln!("  dossiers [-c <config-file>] check [<path-to-spec-directory>]");
}

fn validate_path(path: String) -> Result<PathBuf> {
    let input_path = PathBuf::from(path);
    if !input_path.exists() {
        bail!("Spec source not found: {}", input_path.display());
    }
    Ok(input_path)
}

fn project_root_from(config_path: Option<&Path>, input_path: &Path) -> PathBuf {
    if let Some(path) = config_path {
        if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
    }

    if input_path.is_dir() {
        return input_path.to_path_buf();
    }

    input_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn run_check(input_path: PathBuf, config_path: Option<PathBuf>) -> Result<()> {
    let project_root = project_root_from(config_path.as_deref(), &input_path);
    let (project_config, config_report) =
        validate_project_config_for_check(&project_root, config_path.as_deref());

    let resolved_input = resolve_spec_input_path(&input_path, &project_config);
    let (specs, discovery_report) = discover_specs_for_check(&resolved_input);
    let metadata_report = validate_metadata_for_specs(&specs, &project_config);
    let lint_data = collect_spec_lint_data(&specs);
    let asset_report = validate_asset_references(&specs, &lint_data);
    let cross_report = validate_cross_references(&specs, &lint_data);
    let doc_success = format!(
        "Found {} specification{}",
        specs.len(),
        if specs.len() == 1 { "" } else { "s" }
    );

    let mut total_errors = 0usize;
    let mut total_warnings = 0usize;

    println!("Checking configuration...");
    let (errors, warnings) = print_category_report(&config_report, "Configuration is valid");
    total_errors += errors;
    total_warnings += warnings;

    println!();
    println!("Checking documents...");
    let (errors, warnings) = print_category_report(&discovery_report, &doc_success);
    total_errors += errors;
    total_warnings += warnings;

    println!();
    println!("Checking metadata...");
    let (errors, warnings) = print_category_report(&metadata_report, "All metadata is valid");
    total_errors += errors;
    total_warnings += warnings;

    println!();
    println!("Checking asset references...");
    let (errors, warnings) = print_category_report(&asset_report, "All asset references are valid");
    total_errors += errors;
    total_warnings += warnings;

    println!();
    println!("Checking cross-references...");
    let (errors, warnings) = print_category_report(&cross_report, "All cross-references are valid");
    total_errors += errors;
    total_warnings += warnings;

    println!(
        "Summary: {} errors, {} warnings",
        total_errors, total_warnings
    );

    if total_errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}

fn run_list(input_path: PathBuf, config_path: Option<PathBuf>) -> Result<()> {
    let project_root = project_root_from(config_path.as_deref(), &input_path);
    let project_config = load_project_configuration(&project_root, config_path.as_deref());
    let resolved_input = resolve_spec_input_path(&input_path, &project_config);
    let mut load_result = load_specs(&resolved_input, &project_config)?;
    load_result
        .specs
        .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.title.cmp(&b.title)));

    let display_prefix = project_config.prefix.unwrap_or_default();
    let use_color = supports_color();

    if load_result.specs.is_empty() {
        println!("No specifications found");
        return Ok(());
    }

    for spec in load_result.specs {
        print_list_entry(&spec, &display_prefix, use_color);
        println!();
    }

    Ok(())
}

fn print_list_entry(spec: &SpecDocument, display_prefix: &str, use_color: bool) {
    let bold_start = if use_color { "\u{001b}[1m" } else { "" };
    let bold_end = if use_color { "\u{001b}[22m" } else { "" };
    let bullet = if use_color {
        "\u{001b}[90m•\u{001b}[0m"
    } else {
        "•"
    };

    let display_id = format!("{display_prefix}{}", spec.id);
    println!("{bullet} {display_id} {bold_start}{}{bold_end}", spec.title);

    let status = color_status(&spec.status, use_color);
    let created = format_spec_date(spec.created, false).unwrap_or_else(|| "n/a".into());
    let updated = format_spec_date(spec.updated, false).unwrap_or_else(|| "n/a".into());
    println!("  {status}, created: {created}, updated: {updated}");
}

fn color_status(status: &str, use_color: bool) -> String {
    if !use_color {
        return status.to_string();
    }

    let reset = "\u{001b}[0m";
    let fg = match status.to_ascii_uppercase().as_str() {
        "PUBLISHED" => "\u{001b}[32m",
        "DRAFT" | "REVIEW" => "\u{001b}[33m",
        "ABANDONED" => "\u{001b}[90m",
        _ => "\u{001b}[36m",
    };

    format!("{fg}{status}{reset}")
}

fn print_category_report(report: &CategoryReport, success_message: &str) -> (usize, usize) {
    if report.errors.is_empty() && report.warnings.is_empty() {
        println!("✓ {}", success_message);
        return (0, 0);
    }

    for entry in &report.errors {
        println!("✗ {}", entry.message);
        if let Some(location) = &entry.location {
            println!("  {}", location);
        }
    }

    for entry in &report.warnings {
        println!("! {}", entry.message);
        if let Some(location) = &entry.location {
            println!("  {}", location);
        }
    }

    (report.errors.len(), report.warnings.len())
}

async fn run_server(input_path: PathBuf, config_path: Option<PathBuf>) -> Result<()> {
    let project_root = project_root_from(config_path.as_deref(), &input_path);
    let project_config = load_project_configuration(&project_root, config_path.as_deref());

    let assets = Assets::from_assets_dir(project_root.join("assets"));
    let site_name = resolve_site_name(&project_root, &project_config);

    let (_initial_state, static_mounts) = build_app_state(
        &input_path,
        &project_root,
        site_name,
        assets.clone(),
        project_config,
    )?;
    let reloadable_state = ReloadableAppState {
        input_path: input_path.clone(),
        project_root: project_root.clone(),
        config_path: config_path.clone(),
        assets,
    };

    println!("Serving specs on http://localhost:8080");
    HttpServer::new(move || {
        let mut app = App::new()
            .app_data(web::Data::new(reloadable_state.clone()))
            .route("/", web::get().to(index_page))
            .route("/favicon.svg", web::get().to(favicon))
            .route("/assets/mermaid.min.js", web::get().to(mermaid_script))
            .route("/author/{slug}/", web::get().to(author_redirect))
            .route("/author/{slug}", web::get().to(author_page))
            .route("/status", web::get().to(status_index_page))
            .route("/status/", web::get().to(status_index_page))
            .route("/status/{slug}/", web::get().to(status_redirect))
            .route("/status/{slug}", web::get().to(status_page))
            .route("/{spec_id:\\d+}", web::get().to(spec_page))
            .route("/{spec_id:\\d+}/", web::get().to(spec_redirect));

        for (mount, path) in &static_mounts {
            app = app.service(Files::new(&mount[..], path.clone()).prefer_utf8(true));
        }

        app
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await?;

    Ok(())
}

fn run_prepare(input_path: PathBuf, config_path: Option<PathBuf>) -> Result<()> {
    let project_root = project_root_from(config_path.as_deref(), &input_path);
    let project_config = load_project_configuration(&project_root, config_path.as_deref());
    let (specs, _) = load_and_sort_specs(&input_path, &project_config)?;

    let prepared: Vec<GeneratedSpec> = specs
        .into_iter()
        .map(spec_document_to_generated_spec)
        .collect();

    let output_path = env::current_dir()
        .unwrap_or_else(|_| project_root.clone())
        .join("output.json");

    let file = File::create(&output_path)
        .with_context(|| format!("Creating {}", output_path.display()))?;
    serde_json::to_writer_pretty(file, &prepared)
        .with_context(|| format!("Writing {}", output_path.display()))?;

    println!("Prepared spec data written to {}", output_path.display());
    Ok(())
}

fn run_build(
    input_path: PathBuf,
    output_dir: PathBuf,
    config_path: Option<PathBuf>,
    trailing_slashes: bool,
) -> Result<()> {
    let project_root = project_root_from(config_path.as_deref(), &input_path);
    let project_config = load_project_configuration(&project_root, config_path.as_deref());
    let assets = Assets::embedded();
    let site_name = resolve_site_name(&project_root, &project_config);

    let (mut state, mut static_mounts) = build_app_state(
        &input_path,
        &project_root,
        site_name,
        assets,
        project_config.clone(),
    )?;

    if let Err(err) = augment_with_pull_requests(
        &mut state,
        &mut static_mounts,
        &input_path,
        &project_root,
        &project_config,
    ) {
        eprintln!("Warning: failed to incorporate pull request revisions: {err}");
    }

    state.specs.sort_by(|a, b| {
        b.updated_sort
            .cmp(&a.updated_sort)
            .then_with(|| b.id.cmp(&a.id))
    });

    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)
            .with_context(|| format!("Clearing output directory {}", output_dir.display()))?;
    }
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Creating output directory {}", output_dir.display()))?;

    let mount_map: HashMap<String, PathBuf> = static_mounts.into_iter().collect();

    let index_path = output_dir.join("index.html");
    let index_html = render_index(&state, "./", trailing_slashes).into_string();
    write_html_file(&index_path, index_html)?;
    write_embedded_favicon(&output_dir)?;
    write_mermaid_script(&output_dir, &state.assets.mermaid_script())?;

    for spec in &state.specs {
        let prefix = relative_prefix_for_spec_id(&spec.id);
        let asset_base = if trailing_slashes {
            "".to_string()
        } else {
            join_prefix(&prefix, format!("{}/", spec.id))
        };
        let rendered_html = render_spec_body(&state, spec, asset_base, &prefix, trailing_slashes)?;
        let page =
            render_spec(&state, spec, &rendered_html, &prefix, trailing_slashes).into_string();
        let dest = output_dir.join(&spec.id).join("index.html");
        write_html_file(&dest, page)?;

        let asset_paths = collect_doc_assets(&rendered_html, Some(&spec.id));
        copy_doc_assets(&mount_map, &spec.id, &asset_paths, &output_dir)?;
    }

    let mut authors: HashMap<String, String> = HashMap::new();
    for author in state.specs.iter().flat_map(|spec| spec.authors.iter()) {
        let slug = slugify_author(author);
        authors.entry(slug).or_insert_with(|| author.clone());
    }

    for (slug, name) in authors {
        let authored: Vec<&SpecDocument> = state
            .specs
            .iter()
            .filter(|spec| spec.authors.iter().any(|a| slugify_author(a) == slug))
            .collect();
        let page =
            render_author(&state, &name, &authored, "../../", trailing_slashes).into_string();
        let dest = output_dir.join("author").join(slug).join("index.html");
        write_html_file(&dest, page)?;
    }

    let status_summaries = collect_status_summaries(&state.specs);
    let status_index = render_status_index(&state, &status_summaries, "../").into_string();
    let status_index_path = output_dir.join("status").join("index.html");
    write_html_file(&status_index_path, status_index)?;

    for summary in status_summaries {
        let summary_slug = summary.slug.as_str();
        let matching: Vec<&SpecDocument> = state
            .specs
            .iter()
            .filter(|spec| spec.listed && slugify_status(&spec.status) == summary_slug)
            .collect();
        let page = render_status(&state, &summary.name, &matching, "../../", trailing_slashes)
            .into_string();
        let dest = output_dir
            .join("status")
            .join(&summary.slug)
            .join("index.html");
        write_html_file(&dest, page)?;
    }

    if !index_path.exists() {
        write_html_file(
            &index_path,
            render_index(&state, "./", trailing_slashes).into_string(),
        )?;
    }

    println!(
        "Static site written to {} (index at {})",
        output_dir.display(),
        index_path.display()
    );
    Ok(())
}

fn augment_with_pull_requests(
    state: &mut AppState,
    static_mounts: &mut Vec<StaticMount>,
    input_path: &Path,
    project_root: &Path,
    project_config: &ProjectConfiguration,
) -> Result<()> {
    let token = match env::var("GITHUB_TOKEN") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("Skipping PR revisions: GITHUB_TOKEN not set.");
            return Ok(());
        }
    };

    let git_repo = open_git_repository(project_root);
    let repo_root = git_repo
        .as_ref()
        .map(|repo| repo.workdir().to_path_buf())
        .unwrap_or_else(|| project_root.to_path_buf());

    let repo_from_config = project_config
        .repository
        .as_deref()
        .and_then(parse_github_repo);
    let repo_from_git = git_repo
        .as_ref()
        .and_then(|repo| repo.remote_url())
        .as_deref()
        .and_then(parse_github_repo);

    let Some(github_repo) = repo_from_config.or(repo_from_git) else {
        eprintln!("Skipping PR revisions: no GitHub repository found in config or git remotes.");
        return Ok(());
    };
    let repo_slug = format!("{}/{}", github_repo.owner, github_repo.name);
    if state.github_repo.is_none() {
        state.github_repo = Some(repo_slug.clone());
    }
    eprintln!(
        "Using GitHub repository {}/{} for PR revisions.",
        github_repo.owner, github_repo.name
    );

    let spec_root = resolve_spec_input_path(input_path, project_config);
    let Some(spec_root_relative) = relative_to(&spec_root, &repo_root) else {
        eprintln!(
            "Warning: unable to relate spec root {} to repository root {}; skipping pull request previews.",
            spec_root.display(),
            repo_root.display()
        );
        return Ok(());
    };

    let client = GithubClient::new(github_repo, &token)
        .context("creating GitHub client for pull request previews")?;
    let pulls = client
        .list_open_pulls()
        .context("listing open GitHub pull requests")?;

    if pulls.is_empty() {
        eprintln!("No open pull requests found for preview.");
        return Ok(());
    }
    eprintln!("Found {} open pull request(s).", pulls.len());

    let metadata_reader = MetadataReader::new(project_config.clone());
    for pull in pulls {
        eprintln!("Inspecting PR #{} for revisions...", pull.number);
        let files = match client.list_pull_files(pull.number) {
            Ok(files) => files,
            Err(err) => {
                eprintln!("Warning: skipping PR #{}: {err}", pull.number);
                continue;
            }
        };
        eprintln!(
            "PR #{} contains {} file change(s).",
            pull.number,
            files.len()
        );

        let Some(targets) = map_pull_to_specs(
            &files,
            &spec_root_relative,
            pull.number,
            project_config.pr_number_as_spec_id,
        ) else {
            eprintln!(
                "Skipping PR #{}: no eligible spec changes. Files: {}",
                pull.number,
                files
                    .iter()
                    .map(|f| f.filename.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            continue;
        };
        eprintln!(
            "PR #{} maps to {} spec target(s).",
            pull.number,
            targets.len()
        );

        for target in targets {
            eprintln!(
                "PR #{} -> spec {} at {}",
                pull.number,
                target.spec_id,
                target.primary_relative.display()
            );
            if target.used_pr_id {
                eprintln!(
                    "PR #{} is mapped as a new draft spec using PR number {}.",
                    pull.number, target.spec_id
                );
            }

            if let Err(err) = build_pr_spec_version(
                state,
                static_mounts,
                &client,
                &metadata_reader,
                &pull,
                &files,
                &target.spec_id,
                &target.spec_relative_dir,
                &spec_root,
                &spec_root_relative,
                project_root,
                &target.primary_relative,
            ) {
                eprintln!(
                    "Warning: failed to build PR #{} preview for {}: {err}",
                    pull.number, target.spec_id
                );
            } else {
                let base_spec_exists = state.specs_by_id.contains_key(&target.spec_id);
                if base_spec_exists {
                    eprintln!(
                        "Added PR #{} as revision for existing spec {}.",
                        pull.number, target.spec_id
                    );
                } else {
                    eprintln!(
                        "Added PR #{} as new spec {} (listed in index).",
                        pull.number, target.spec_id
                    );
                }
            }
        }
    }

    for revisions in state.revisions.values_mut() {
        revisions.sort_by_key(|rev| rev.pr_number);
    }

    Ok(())
}

struct SpecTarget {
    spec_id: String,
    spec_relative_dir: PathBuf,
    primary_relative: PathBuf,
    used_pr_id: bool,
}

fn map_pull_to_specs(
    files: &[GithubFile],
    spec_root_relative: &Path,
    pr_number: u64,
    pr_number_as_spec_id: bool,
) -> Option<Vec<SpecTarget>> {
    let mut ignored_non_spec = 0usize;
    let pr_id = format!("{pr_number:04}");
    let mut targets: Vec<SpecTarget> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for file in files {
        let repo_path = Path::new(&file.filename);
        let Some((current_id, relative_path)) =
            spec_id_from_repo_path(repo_path, spec_root_relative)
        else {
            ignored_non_spec += 1;
            continue;
        };
        let target_id = if pr_number_as_spec_id && (current_id == "0000" || current_id == pr_id) {
            pr_id.clone()
        } else {
            current_id.clone()
        };

        let target_dir =
            spec_dir_for_relative_path(&relative_path, &current_id).unwrap_or_default();

        if seen_ids.insert(target_id.clone()) {
            targets.push(SpecTarget {
                spec_id: target_id,
                spec_relative_dir: target_dir.clone(),
                primary_relative: target_dir,
                used_pr_id: pr_number_as_spec_id && (current_id == "0000" || current_id == pr_id),
            });
        }
    }

    if targets.is_empty() {
        eprintln!(
            "Skipping PR mapping: no spec files under {} were touched (ignored {} non-spec file(s)).",
            spec_root_relative.display(),
            ignored_non_spec
        );
        return None;
    }

    Some(targets)
}

#[allow(clippy::too_many_arguments)]
fn build_pr_spec_version(
    state: &mut AppState,
    static_mounts: &mut Vec<StaticMount>,
    client: &GithubClient,
    metadata_reader: &MetadataReader,
    pull: &GithubPull,
    files: &[GithubFile],
    spec_id: &str,
    spec_relative_dir: &Path,
    spec_root: &Path,
    spec_root_relative: &Path,
    project_root: &Path,
    primary_relative: &Path,
) -> Result<()> {
    let workspace_root = project_root
        .join("target")
        .join("pr-previews")
        .join(format!("pr-{}", pull.number));
    if workspace_root.exists() {
        let _ = fs::remove_dir_all(&workspace_root);
    }
    let spec_root_temp = workspace_root.join("specs");
    let target_doc = spec_root_temp.join(primary_relative);
    if let Some(parent) = target_doc.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating workspace for PR #{}", pull.number))?;
    } else {
        fs::create_dir_all(&spec_root_temp)
            .with_context(|| format!("creating workspace for PR #{}", pull.number))?;
    }

    let local_doc = spec_root.join(primary_relative);
    if local_doc.exists() && local_doc.is_file() {
        fs::copy(&local_doc, &target_doc)
            .with_context(|| format!("copying baseline document {}", local_doc.display()))?;
    } else if !spec_relative_dir.as_os_str().is_empty() {
        let local_spec_dir = spec_root.join(spec_relative_dir);
        if local_spec_dir.exists() && local_spec_dir.is_dir() {
            copy_dir_contents(&local_spec_dir, &spec_root_temp.join(spec_relative_dir))?;
        }
    }

    for file in files {
        if let Some(previous) = file.previous_filename.as_ref() {
            let previous_path = Path::new(previous);
            if let Some(relative) = relative_to_spec_root(previous_path, spec_root_relative) {
                let target = spec_root_temp.join(relative);
                let _ = fs::remove_file(&target);
            }
        }

        let repo_path = Path::new(&file.filename);
        let Some(relative) = relative_to_spec_root(repo_path, spec_root_relative) else {
            continue;
        };
        let target_path = spec_root_temp.join(relative);

        if file.status == "removed" {
            let _ = fs::remove_file(&target_path);
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let bytes = if let Some(raw_url) = file.raw_url.as_deref() {
            match client.download_bytes(raw_url) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!(
                        "Warning: raw download failed for {} (PR #{}): {err}; falling back to contents API.",
                        file.filename, pull.number
                    );
                    client.download_file_at_ref(&file.filename, &pull.head_sha)?
                }
            }
        } else {
            client.download_file_at_ref(&file.filename, &pull.head_sha)?
        };

        fs::write(&target_path, &bytes)
            .with_context(|| format!("writing PR file {}", target_path.display()))?;
    }

    let doc_root = spec_root_temp.join(primary_relative);
    let (doc_path, format) = if doc_root.is_dir() {
        find_doc_file(&doc_root)?
    } else if doc_root.is_file() {
        let ext = doc_root
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default();
        let format = match ext.as_str() {
            "md" | "markdown" => DocFormat::Markdown,
            "adoc" | "asciidoc" => DocFormat::Asciidoc,
            _ => bail!(
                "Unsupported document format for PR #{} at {}",
                pull.number,
                doc_root.display()
            ),
        };
        (doc_root.clone(), format)
    } else {
        bail!(
            "No document found for PR #{} in {}",
            pull.number,
            doc_root.display()
        );
    };

    let dir_name = spec_relative_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(spec_id)
        .to_string();
    let display_name = display_name_from_dir(&dir_name);
    let source = fs::read_to_string(&doc_path)
        .with_context(|| format!("reading PR document {}", doc_path.display()))?;
    let parsed = metadata_reader.read(&source, format, &display_name);
    let meta = parsed.metadata;
    let status_fallback = if pull.draft {
        "DRAFT".to_string()
    } else {
        "REVIEW".to_string()
    };

    let meta_created = meta.created.as_deref().and_then(parse_date);
    let meta_updated = meta.updated.as_deref().and_then(parse_date);
    let authors = if meta.authors.is_empty() {
        pull.author
            .as_ref()
            .map(|a| vec![a.clone()])
            .unwrap_or_default()
    } else {
        meta.authors.clone()
    };
    let base_created = state.specs_by_id.get(spec_id).and_then(|spec| spec.created);
    let base_updated = state.specs_by_id.get(spec_id).and_then(|spec| spec.updated);
    let base_exists = state.specs_by_id.contains_key(spec_id);
    let (file_created, file_modified) = file_timestamps(&doc_path);

    let created = meta_created
        .or(Some(pull.created_at))
        .or(base_created)
        .or(file_created)
        .or(file_modified);
    let updated = meta_updated
        .or(Some(pull.updated_at))
        .or(base_updated)
        .or(file_modified)
        .or(created)
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    let updated_sort = updated;

    let status = meta.status.unwrap_or(status_fallback);
    let pr_id = if base_exists {
        format!("{}/pr/{}", spec_id, pull.number)
    } else {
        spec_id.to_string()
    };
    let pr_spec = SpecDocument {
        id: pr_id.clone(),
        dir_name,
        title: meta
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| display_name.clone()),
        status,
        created,
        updated: Some(updated),
        authors,
        links: meta.links,
        updated_sort,
        extra: metadata_extra_to_json(&meta.extra),
        source: parsed.body,
        format,
        listed: !base_exists,
        revision_of: base_exists.then(|| spec_id.to_string()),
        pr_number: Some(pull.number),
    };

    let mount_path = if base_exists {
        format!("/{}/pr/{}", spec_id, pull.number)
    } else {
        format!("/{spec_id}")
    };
    let static_root = if doc_root.is_dir() {
        doc_root.clone()
    } else {
        doc_root.parent().unwrap_or(&doc_root).to_path_buf()
    };
    static_mounts.push((mount_path, static_root));
    insert_spec_document(state, pr_spec.clone());

    if base_exists {
        state
            .revisions
            .entry(spec_id.to_string())
            .or_default()
            .push(RevisionLink {
                pr_number: pull.number,
                status: pr_spec.status.clone(),
                updated: pr_spec.updated,
                href: pr_spec.id.clone(),
            });
    }

    Ok(())
}

fn spec_id_from_repo_path(path: &Path, spec_root_relative: &Path) -> Option<(String, PathBuf)> {
    let stripped = relative_to_spec_root(path, spec_root_relative)?;
    for component in stripped.components() {
        if let Component::Normal(os) = component {
            let Some(name) = os.to_str() else { continue };
            if let Some(id) = extract_spec_id(name) {
                return Some((id, stripped));
            }
        }
    }

    None
}

fn spec_dir_for_relative_path(path: &Path, spec_id: &str) -> Option<PathBuf> {
    let mut accum = PathBuf::new();
    let components = path.components().peekable();

    for component in components {
        let Component::Normal(os) = component else {
            continue;
        };
        let name = os.to_str()?;
        if let Some(id) = extract_spec_id(name) {
            if id == spec_id {
                accum.push(name);
                return Some(accum);
            }
        }
        accum.push(name);
    }

    None
}

fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    let base = base.canonicalize().ok()?;
    path.strip_prefix(base).map(|p| p.to_path_buf()).ok()
}

fn relative_to_spec_root(path: &Path, spec_root_relative: &Path) -> Option<PathBuf> {
    if path.starts_with(spec_root_relative) {
        if let Ok(stripped) = path.strip_prefix(spec_root_relative) {
            return Some(stripped.to_path_buf());
        }
    }

    if let Some(last) = spec_root_relative.components().next_back() {
        let tail = PathBuf::from(last.as_os_str());
        if path.starts_with(&tail) {
            if let Ok(stripped) = path.strip_prefix(&tail) {
                return Some(stripped.to_path_buf());
            }
        }
    }

    None
}

fn build_app_state(
    input_path: &Path,
    project_root: &Path,
    site_name: String,
    assets: Assets,
    project_config: ProjectConfiguration,
) -> Result<(AppState, Vec<StaticMount>)> {
    let (specs, static_mounts) = load_and_sort_specs(input_path, &project_config)?;
    let generated_at = chrono::Utc::now().timestamp_millis();
    let spec_ids = specs.iter().map(|s| s.id.clone()).collect::<HashSet<_>>();
    let renderer = DocRenderer::new();
    let specs_by_id = specs
        .iter()
        .cloned()
        .map(|spec| (spec.id.clone(), spec))
        .collect::<HashMap<_, _>>();

    // Try config first, then fall back to git remote
    let repo_from_config = project_config
        .repository
        .as_deref()
        .and_then(parse_github_repo);
    let git_repo = open_git_repository(project_root);
    let repo_from_git = git_repo
        .as_ref()
        .and_then(|repo| repo.remote_url())
        .as_deref()
        .and_then(parse_github_repo);
    let github_repo = repo_from_config
        .or(repo_from_git)
        .map(|repo| format!("{}/{}", repo.owner, repo.name));

    // Compute create config if GitHub repo is available
    let spec_root = resolve_spec_input_path(input_path, &project_config);
    let create_config = github_repo.as_ref().map(|repo| {
        // Find the next available spec ID by parsing existing IDs as numbers
        let next_id = spec_ids
            .iter()
            .filter_map(|id| id.parse::<u64>().ok())
            .max()
            .map(|max| max + 1)
            .unwrap_or(1);

        // Infer format from existing specs (majority wins), or use config, or default to markdown
        let format = project_config
            .new_document_format
            .as_deref()
            .map(|f| if f == "asciidoc" { "adoc" } else { "md" })
            .unwrap_or_else(|| {
                let asciidoc_count = specs
                    .iter()
                    .filter(|s| matches!(s.format, DocFormat::Asciidoc))
                    .count();
                let markdown_count = specs.len().saturating_sub(asciidoc_count);
                if asciidoc_count > markdown_count {
                    "adoc"
                } else {
                    "md"
                }
            })
            .to_string();

        // Infer structure from existing specs (check if dir_name corresponds to a directory)
        let structure = project_config
            .new_document_structure
            .clone()
            .unwrap_or_else(|| {
                let dir_count = specs
                    .iter()
                    .filter(|s| spec_root.join(&s.dir_name).is_dir())
                    .count();
                if dir_count > specs.len() / 2 {
                    "directory".to_string()
                } else {
                    "flat".to_string()
                }
            });

        CreateDocConfig {
            github_repo: repo.clone(),
            default_branch: project_config
                .default_branch
                .clone()
                .unwrap_or_else(|| "main".to_string()),
            subdirectory: project_config.subdirectory.clone(),
            next_id,
            format,
            structure,
        }
    });

    let state = AppState {
        specs,
        specs_by_id,
        spec_ids,
        revisions: HashMap::new(),
        display_prefix: project_config.prefix.clone().unwrap_or_default(),
        site_name,
        site_description: project_config.description.unwrap_or_default(),
        github_repo,
        extra_fields: project_config.extra_metadata_fields.clone(),
        generated_at,
        assets,
        renderer,
        create_config,
    };

    Ok((state, static_mounts))
}

fn copy_dir_contents(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_contents(&path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target_path).with_context(|| {
                format!(
                    "Copying file {} to {}",
                    path.display(),
                    target_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn write_embedded_favicon(output_root: &Path) -> Result<()> {
    let target = output_root.join("favicon.svg");
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, EMBEDDED_FAVICON)
        .with_context(|| format!("Writing favicon to {}", target.display()))
}

fn write_mermaid_script(output_root: &Path, mermaid_js: &str) -> Result<()> {
    if mermaid_js.trim().is_empty() {
        return Ok(());
    }

    let target = output_root.join("assets").join("mermaid.min.js");
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, mermaid_js)
        .with_context(|| format!("Writing mermaid script to {}", target.display()))
}

fn collect_doc_assets(html: &str, spec_id: Option<&str>) -> Vec<String> {
    lazy_static! {
        static ref ASSET_RE: Regex = Regex::new(r#"(?i)\b(?:src|href)=['"]([^'">]+)['"]"#).unwrap();
    }

    ASSET_RE
        .captures_iter(html)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .map(|raw| normalize_asset_path(&raw, spec_id))
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_asset_path(raw: &str, spec_id: Option<&str>) -> String {
    if raw.is_empty() || raw.starts_with('#') || raw.starts_with('/') || raw.starts_with("//") {
        return String::new();
    }

    lazy_static! {
        static ref SCHEME_RE: Regex = Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*:").unwrap();
    }

    if SCHEME_RE.is_match(raw) {
        return String::new();
    }

    let without_query = raw
        .split(['?', '#'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();

    let mut path = without_query
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    while path.starts_with("../") {
        path = path.trim_start_matches("../").to_string();
    }
    if let Some(spec_id) = spec_id {
        if let Some(stripped) = path.strip_prefix(spec_id) {
            path = stripped.trim_start_matches('/').to_string();
        }
    }

    path
}

fn file_timestamps(path: &Path) -> (Option<i64>, Option<i64>) {
    let Ok(metadata) = fs::metadata(path) else {
        return (None, None);
    };

    let created = metadata.created().ok().and_then(system_time_to_millis);
    let modified = metadata.modified().ok().and_then(system_time_to_millis);

    (created.or(modified), modified)
}

fn system_time_to_millis(time: SystemTime) -> Option<i64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis().try_into().unwrap_or(i64::MAX))
}

fn copy_doc_assets(
    mounts: &HashMap<String, PathBuf>,
    spec_id: &str,
    asset_paths: &[String],
    output_root: &Path,
) -> Result<()> {
    if asset_paths.is_empty() {
        return Ok(());
    }

    let mount_key = format!("/{spec_id}");
    let Some(source_root) = mounts.get(&mount_key) else {
        eprintln!("Warning: no static mount found for spec {}", spec_id);
        return Ok(());
    };

    for asset in asset_paths {
        if asset.is_empty() {
            continue;
        }

        let source = source_root.join(asset);
        if !source.exists() {
            eprintln!(
                "Warning: referenced asset not found for spec {}: {}",
                spec_id,
                source.display()
            );
            continue;
        }

        let target_root = output_root.join(spec_id);
        let target = target_root.join(asset);

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        if source.is_dir() {
            copy_dir_contents(&source, &target)?;
        } else {
            fs::copy(&source, &target).with_context(|| {
                format!("Copying asset {} to {}", source.display(), target.display())
            })?;
        }
    }

    Ok(())
}

fn write_html_file(path: &Path, content: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Creating directory {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("Writing {}", path.display()))
}

async fn favicon(state: web::Data<ReloadableAppState>) -> impl Responder {
    let favicon = state.assets().favicon();
    HttpResponse::Ok()
        .content_type("image/svg+xml")
        .body(favicon)
}

async fn mermaid_script(state: web::Data<ReloadableAppState>) -> impl Responder {
    let mermaid_js = state.assets().mermaid_script();
    HttpResponse::Ok()
        .content_type("text/javascript; charset=utf-8")
        .body(mermaid_js)
}

async fn index_page(state: web::Data<ReloadableAppState>) -> impl Responder {
    match state.load() {
        Ok(loaded) => {
            let markup = render_index(&loaded, "/", false);
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(markup.into_string())
        }
        Err(err) => {
            eprintln!("Failed to load specs for index: {err:?}");
            HttpResponse::InternalServerError()
                .body(format!("Failed to load specifications: {err}"))
        }
    }
}

async fn spec_redirect(path: web::Path<String>) -> impl Responder {
    let spec_id = path.into_inner();
    HttpResponse::MovedPermanently()
        .append_header(("Location", format!("/{spec_id}")))
        .finish()
}

async fn spec_page(
    path: web::Path<String>,
    state: web::Data<ReloadableAppState>,
) -> impl Responder {
    let spec_id = path.into_inner();
    let loaded = match state.load() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("Failed to load specs for {spec_id}: {err:?}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to load specification {spec_id}: {err}"));
        }
    };

    let Some(spec) = loaded.specs_by_id.get(&spec_id) else {
        return HttpResponse::Found()
            .append_header(("Location", "/"))
            .finish();
    };

    let rendered_html = match render_spec_body(&loaded, spec, format!("/{}/", spec.id), "/", false)
    {
        Ok(html) => html,
        Err(err) => {
            eprintln!("Failed to render spec {spec_id}: {err:?}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to render specification {spec_id}: {err:?}"));
        }
    };

    let markup = render_spec(&loaded, spec, &rendered_html, "/", false);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

async fn author_redirect(path: web::Path<String>) -> impl Responder {
    let slug = path.into_inner();
    HttpResponse::MovedPermanently()
        .append_header(("Location", format!("/author/{slug}")))
        .finish()
}

async fn author_page(
    path: web::Path<String>,
    state: web::Data<ReloadableAppState>,
) -> impl Responder {
    let slug = path.into_inner();
    let loaded = match state.load() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("Failed to load specs for author page: {err:?}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to load author page: {err}"));
        }
    };
    let authored: Vec<&SpecDocument> = loaded
        .specs
        .iter()
        .filter(|spec| {
            spec.listed
                && spec
                    .authors
                    .iter()
                    .any(|author| slugify_author(author) == slug)
        })
        .collect();

    let author_name = authored
        .iter()
        .flat_map(|spec| spec.authors.iter())
        .find(|name| slugify_author(name) == slug)
        .cloned()
        .unwrap_or_else(|| slug.clone());

    let markup = render_author(&loaded, &author_name, &authored, "/", false);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

async fn status_index_page(state: web::Data<ReloadableAppState>) -> impl Responder {
    match state.load() {
        Ok(loaded) => {
            let summaries = collect_status_summaries(&loaded.specs);
            let markup = render_status_index(&loaded, &summaries, "/");
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(markup.into_string())
        }
        Err(err) => {
            eprintln!("Failed to load specs for status index: {err:?}");
            HttpResponse::InternalServerError().body(format!("Failed to load status index: {err}"))
        }
    }
}

async fn status_redirect(path: web::Path<String>) -> impl Responder {
    let slug = path.into_inner();
    HttpResponse::MovedPermanently()
        .append_header(("Location", format!("/status/{slug}")))
        .finish()
}

async fn status_page(
    path: web::Path<String>,
    state: web::Data<ReloadableAppState>,
) -> impl Responder {
    let slug = path.into_inner();
    let slug_value = slug.as_str();
    let loaded = match state.load() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("Failed to load specs for status page: {err:?}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to load status page: {err}"));
        }
    };
    let matching: Vec<&SpecDocument> = loaded
        .specs
        .iter()
        .filter(|spec| spec.listed && slugify_status(&spec.status) == slug_value)
        .collect();

    let status_name = matching
        .first()
        .map(|spec| spec.status.clone())
        .unwrap_or_else(|| slug.clone());

    let markup = render_status(&loaded, &status_name, &matching, "/", false);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_index(state: &AppState, prefix: &str, trailing_slashes: bool) -> Markup {
    let site_name = &state.site_name;
    let index_search_js = state.assets.index_search_script();
    let listed_specs: Vec<&SpecDocument> = state.specs.iter().filter(|spec| spec.listed).collect();
    let create_config = &state.create_config;
    let content = html! {
        main class="container" {
            section class="hero" {
                div class="hero-header" {
                    h1 { "Specification Library" }
                    @if let Some(config) = create_config {
                        button type="button" class="create-btn" onclick="openCreateModal()" { "+ Create..." }
                        // Hidden data for JS
                        input type="hidden" id="create-github-repo" value=(config.github_repo) {}
                        input type="hidden" id="create-branch" value=(config.default_branch) {}
                        input type="hidden" id="create-subdir" value=(config.subdirectory.as_deref().unwrap_or("")) {}
                        input type="hidden" id="create-next-id" value=(format!("{:04}", config.next_id)) {}
                        input type="hidden" id="create-format" value=(config.format) {}
                        input type="hidden" id="create-structure" value=(config.structure) {}
                    }
                }
                p { "Browse all specifications documents. Search by title, ID, or author to jump straight to what you need." }
                form class="search-bar" role="search" onsubmit="event.preventDefault();" {
                    label class="sr-only" for="spec-search" { "Search specifications" }
                    div class="search-input" {
                        input id="spec-search" type="search" name="q" placeholder="Search by title, ID, or author" autocomplete="off" autofocus {}
                        span class="search-hint" { "/" }
                    }
                }
            }

            @if listed_specs.is_empty() {
                p class="empty-state" { "No specification documents found." }
            } @else {
                ul class="spec-list" {
                    @for spec in &listed_specs {
                        @let base_id = spec.revision_of.as_deref().unwrap_or(&spec.id);
                        li
                            data-title={(spec.title.to_lowercase())}
                            data-id={(base_id.to_lowercase())}
                            data-authors={(spec.authors.iter().map(|a| a.to_lowercase()).collect::<Vec<_>>().join(" "))}
                        {
                            a class="spec-card" href={(join_spec_link(prefix, &spec.id, "", trailing_slashes))} {
                                div class="spec-meta" {
                                    span class="spec-id" { "#" (base_id) }
                                }
                                div class="spec-title" { (&spec.title) }
                                div class="spec-meta-details" {
                                    span class={(format!("tag {}", spec.status.to_lowercase()))} { (&spec.status) }
                                    span { "Created: " (format_spec_date(spec.created, false).unwrap_or_else(|| "n/a".into())) }
                                    span { "Updated: " (format_spec_date(spec.updated, false).unwrap_or_else(|| "n/a".into())) }
                                }
                            }
                        }
                    }
                }
                p class="empty-state filter-empty" hidden { "No specs match this search." }
            }

            // Create document modal
            @if create_config.is_some() {
                div id="create-modal" class="modal" hidden {
                    div class="modal-backdrop" onclick="closeCreateModal()" {}
                    div class="modal-content" {
                        h2 { "Create New Specification" }
                        form id="create-form" onsubmit="handleCreateSubmit(event)" {
                            label for="create-title" { "Document Title" }
                            input type="text" id="create-title" name="title" placeholder="My New Specification" required autofocus {}
                            p class="create-preview" {
                                "Path: "
                                code id="create-path-preview" { "..." }
                            }
                            div class="modal-actions" {
                                button type="button" class="btn-secondary" onclick="closeCreateModal()" { "Cancel" }
                                button type="submit" class="btn-primary" { "Create on GitHub" }
                            }
                        }
                    }
                }
            }
            }
        script { (PreEscaped(index_search_js)) }
        @if create_config.is_some() {
            script { (PreEscaped(CREATE_MODAL_JS)) }
        }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    base_layout(
        site_name,
        &state.site_description,
        site_name,
        &state.site_description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
            mermaid_js_url: None,
            mermaid_init_js: None,
        },
        content,
        prefix,
        state.generated_at,
    )
}

fn render_spec(
    state: &AppState,
    spec: &SpecDocument,
    rendered_html: &str,
    prefix: &str,
    trailing_slashes: bool,
) -> Markup {
    let base_id = spec.revision_of.clone().unwrap_or_else(|| spec.id.clone());
    let display_id = format_display_id(&state.display_prefix, &base_id);
    let page_id_label = format!("#{base_id}");
    let title_id = if let Some(pr_number) = spec.pr_number {
        format!("{display_id} PR #{pr_number}")
    } else {
        display_id.clone()
    };
    let title = format!("{title_id} {} - {}", spec.title, state.site_name);
    let description = format!("Rendered specification {}", spec.dir_name);
    let pr_review = match (spec.pr_number, state.github_repo.as_deref()) {
        (Some(pr_number), Some(repo_slug)) => Some((
            format!("{repo_slug}#{pr_number}"),
            format!("https://github.com/{repo_slug}/pull/{pr_number}"),
        )),
        _ => None,
    };
    let links: Vec<(&str, &str)> = spec
        .links
        .iter()
        .map(|link| (link.label.as_str(), link.href.as_str()))
        .collect();
    let extra_pairs = state
        .extra_fields
        .iter()
        .filter_map(|field| {
            spec.extra.get(&field.name).map(|value| {
                let label = field
                    .display_name
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| field.name.clone());
                let display = display_extra_value(value);
                let is_html = field.type_hint == MetadataValueType::Markdown;
                let link = match (&field.link_format, value, field.type_hint) {
                    (Some(fmt), Value::String(raw), MetadataValueType::String)
                        if !raw.is_empty() =>
                    {
                        let encoded = url_escape_component(raw);
                        Some(fmt.replace("{value}", &encoded))
                    }
                    _ => None,
                };
                (label, display, link, is_html)
            })
        })
        .filter(|(_, v, _, _)| !v.is_empty())
        .collect::<Vec<_>>();
    let revisions = state.revisions.get(&base_id);
    let is_pr_page = spec.pr_number.is_some();
    let original_link = spec.revision_of.as_ref().map(|id| {
        let display = format_display_id(&state.display_prefix, id);
        let href = join_spec_link(prefix, id, "", trailing_slashes);
        (display, href)
    });

    let mini_toc_js = state.assets.mini_toc_script();
    let needs_mermaid = has_mermaid_markup(rendered_html);
    let mermaid_js_url = if needs_mermaid {
        Some(join_prefix(prefix, "assets/mermaid.min.js"))
    } else {
        None
    };
    let mermaid_init_js = if needs_mermaid {
        Some(state.assets.mermaid_init_script())
    } else {
        None
    };
    let content = html! {
        main class="container" {
            a class="back-link" href={(join_prefix(prefix, ""))} { "← Back to index" }

            div class="spec-header" {
                span class="meta-label" { "" }
                a
                    class={(format!("tag {}", spec.status.to_lowercase()))}
                    href={(join_prefix(prefix, format!("status/{}", slugify_status(&spec.status))))}
                {
                    (&spec.status)
                }
                @if let Some((pr_text, pr_href)) = pr_review.as_ref() {
                    span { "—" }
                    span {
                       "Review here: "
                       a class="spec-metadata-link" href=(pr_href) target="_blank" rel="noreferrer noopener" { (pr_text) }
                    }
                }
            }
            div class="spec-header" {
                div class="spec-id-block" { span class="spec-id" { (page_id_label) } }
                div class="spec-title-block" {
                    h1 id="doc-top" { (&spec.title) }
                }
            }

            @if !spec.authors.is_empty() {
                div class="spec-header" {
                    span class="meta-label" { "Author" @if spec.authors.len() > 1 { "s" } }
                    span {
                        @for (index, author) in spec.authors.iter().enumerate() {
                            @if index > 0 { span class="meta-divider" { "•" } }
                            a class="spec-author-link" href={(join_prefix(prefix, format!("author/{}", slugify_author(author))))} { (author) }
                        }
                    }
                }
            }

            div class="spec-header" {
                span class="meta-label" { "Created" }
                span { (format_spec_date(spec.created, true).unwrap_or_else(|| "n/a".into())) }
            }
            div class="spec-header" {
                span class="meta-label" { "Updated" }
                span { (format_spec_date(spec.updated, true).unwrap_or_else(|| "n/a".into())) }
            }

            @if !links.is_empty() {
                div class="spec-header" {
                    span class="meta-label" { "Links" }
                    span {
                        @for (index, (label, href)) in links.iter().enumerate() {
                            @if index > 0 { span class="meta-divider" { "•" } }
                            a class="spec-metadata-link" href=(*href) target="_blank" rel="noreferrer noopener" { (label) }
                        }
                    }
                }
            }

            @for (key, value, link, is_html) in extra_pairs {
                div class="spec-header" {
                    span class="meta-label" { (key) }
                    @if let Some(href) = link {
                        a class="spec-metadata-link meta-value" href=(href) target="_blank" rel="noreferrer noopener" { (value) }
                    } @else if is_html {
                        div class="meta-value meta-value--markdown" { (PreEscaped(value)) }
                    } @else {
                        span class="meta-value" { (value) }
                    }
                }
            }

            @if is_pr_page {
                @if let Some((display, href)) = original_link.as_ref() {
                    div class="spec-header" {
                        span class="meta-label" { "ORIGINAL" }
                        a class="spec-metadata-link" href=(href) { (display) }
                    }
                }
            } @else if let Some(items) = revisions {
                @if !items.is_empty() {
                    div class="spec-header" {
                        span class="meta-label" { "REVISIONS" }
                        span {
                            @for (index, revision) in items.iter().enumerate() {
                                @if index > 0 { span class="meta-divider" { "•" } }
                                a class="spec-metadata-link" href={(join_spec_link(prefix, revision.href.trim_start_matches('/'), "", trailing_slashes))} {
                                    (format!("PR #{}", revision.pr_number))
                                }
                                span class="meta-note" {
                                    " (last update: "
                                    (format_iso_date(revision.updated).unwrap_or_else(|| "n/a".into()))
                                    ", status: "
                                    (&revision.status)
                                    ")"
                                }
                            }
                        }
                    }
                }
            }

            div class="doc-layout" {
                article class="doc-content" { (PreEscaped(rendered_html)) }
                nav class="mini-toc" aria-label="Contents" {
                    div class="mini-toc__title" { "Contents" }
                    ol class="mini-toc__list" {}
                }
            }
            }
        script { (PreEscaped(mini_toc_js)) }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    let mermaid_init_js = mermaid_init_js.as_deref();
    base_layout(
        &state.site_name,
        &state.site_description,
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
            mermaid_js_url: mermaid_js_url.as_deref(),
            mermaid_init_js,
        },
        content,
        prefix,
        state.generated_at,
    )
}

fn render_author(
    state: &AppState,
    author_name: &str,
    authored: &[&SpecDocument],
    prefix: &str,
    trailing_slashes: bool,
) -> Markup {
    let title = format!("{author_name} - {}", state.site_name);
    let description = format!("All specs attributed to {author_name}");

    let content = html! {
        main class="container" {
            a class="back-link" href={(join_prefix(prefix, ""))} { "← Back to index" }

            div class="spec-header" {
                h1 { "Specs by " (author_name) }
                span class="spec-dir" { (format!("{} spec{}", authored.len(), if authored.len() == 1 { "" } else { "s" })) }
            }

            @if authored.is_empty() {
                p class="empty-state" { "No specs found for this author." }
            } @else {
                ul class="spec-list" {
                    @for spec in authored {
                        li {
                            a class="spec-card" href={(join_spec_link(prefix, &spec.id, "", trailing_slashes))} {
                                div class="spec-meta" {
                                span class="spec-id" { "#" (spec.id) }
                                }
                                div class="spec-title" { (&spec.title) }
                                div class="spec-meta-details" {
                                    span class={(format!("tag {}", spec.status.to_lowercase()))} { (&spec.status) }
                                    span { "Created: " (format_spec_date(spec.created, false).unwrap_or_else(|| "n/a".into())) }
                                    span { "Updated: " (format_spec_date(spec.updated, false).unwrap_or_else(|| "n/a".into())) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    base_layout(
        &state.site_name,
        &state.site_description,
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
            mermaid_js_url: None,
            mermaid_init_js: None,
        },
        content,
        prefix,
        state.generated_at,
    )
}

fn render_status(
    state: &AppState,
    status_name: &str,
    matching: &[&SpecDocument],
    prefix: &str,
    trailing_slashes: bool,
) -> Markup {
    let title = format!("{status_name} specs - {}", state.site_name);
    let description = format!("All specs with status {status_name}");
    let index_search_js = state.assets.index_search_script();

    let content = html! {
        main class="container" {
            div class="page-nav" {
                a class="back-link" href={(join_prefix(prefix, ""))} { "← Back to index" }
                a class="back-link" href={(join_prefix(prefix, "status"))} { "All statuses" }
            }

            section class="hero" {
                h1 { "Status: " (status_name) }
                p { "Browse specs with this status. Search by title, ID, or author." }
                form class="search-bar" role="search" onsubmit="event.preventDefault();" {
                    label class="sr-only" for="spec-search" { "Search specifications" }
                    div class="search-input" {
                        input id="spec-search" type="search" name="q" placeholder="Search by title, ID, or author" autocomplete="off" {}
                        span class="search-hint" { "/" }
                    }
                }
            }
            div class="spec-header" {
                span class={(format!("tag {}", status_name.to_lowercase()))} { (status_name) }
                span class="spec-dir" { (format!("{} spec{}", matching.len(), if matching.len() == 1 { "" } else { "s" })) }
            }

            @if matching.is_empty() {
                p class="empty-state" { "No specs found for this status." }
            } @else {
                ul class="spec-list" {
                    @for spec in matching {
                        @let base_id = spec.revision_of.as_deref().unwrap_or(&spec.id);
                        li
                            data-title={(spec.title.to_lowercase())}
                            data-id={(base_id.to_lowercase())}
                            data-authors={(spec.authors.iter().map(|a| a.to_lowercase()).collect::<Vec<_>>().join(" "))}
                        {
                            a class="spec-card" href={(join_spec_link(prefix, &spec.id, "", trailing_slashes))} {
                                div class="spec-meta" {
                                span class="spec-id" { "#" (base_id) }
                                }
                                div class="spec-title" { (&spec.title) }
                                div class="spec-meta-details" {
                                    span class={(format!("tag {}", spec.status.to_lowercase()))} { (&spec.status) }
                                    span { "Created: " (format_spec_date(spec.created, false).unwrap_or_else(|| "n/a".into())) }
                                    span { "Updated: " (format_spec_date(spec.updated, false).unwrap_or_else(|| "n/a".into())) }
                                }
                            }
                        }
                    }
                }
                p class="empty-state filter-empty" hidden { "No specs match this search." }
            }
            }
        script { (PreEscaped(index_search_js)) }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    base_layout(
        &state.site_name,
        &state.site_description,
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
            mermaid_js_url: None,
            mermaid_init_js: None,
        },
        content,
        prefix,
        state.generated_at,
    )
}

fn render_status_index(state: &AppState, summaries: &[StatusSummary], prefix: &str) -> Markup {
    let title = format!("Statuses - {}", state.site_name);
    let description = "Browse specs by status.".to_string();

    let content = html! {
        main class="container" {
            a class="back-link" href={(join_prefix(prefix, ""))} { "← Back to index" }

            section class="hero" {
                h1 { "Statuses" }
                p { "Browse specification documents grouped by status." }
            }

            @if summaries.is_empty() {
                p class="empty-state" { "No statuses found." }
            } @else {
                ul class="spec-list" {
                    @for summary in summaries {
                        li {
                            a class="spec-card" href={(join_prefix(prefix, format!("status/{}", summary.slug.as_str())))} {
                                div class="spec-meta" {
                                    span class="spec-id" { (summary.count) }
                                    span class="spec-dir" { (format!("document{}", if summary.count == 1 { "" } else { "s" })) }
                                }
                                div class="spec-title" { (&summary.name) }
                                div class="spec-meta-details" {
                                    span class={(format!("tag {}", summary.name.to_lowercase()))} { (&summary.name) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    base_layout(
        &state.site_name,
        &state.site_description,
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
            mermaid_js_url: None,
            mermaid_init_js: None,
        },
        content,
        prefix,
        state.generated_at,
    )
}

fn join_prefix(prefix: &str, path: impl AsRef<str>) -> String {
    let trimmed = path.as_ref().trim_start_matches('/');
    if prefix.is_empty() {
        if trimmed.is_empty() {
            ".".into()
        } else {
            trimmed.to_string()
        }
    } else {
        let normalized_prefix = if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{prefix}/")
        };
        if trimmed.is_empty() {
            if prefix == "/" {
                "/".into()
            } else {
                normalized_prefix.trim_end_matches('/').to_string()
            }
        } else {
            format!("{normalized_prefix}{trimmed}")
        }
    }
}

fn build_spec_path(spec_id: &str, fragment: &str, trailing_slashes: bool) -> String {
    let base = spec_id.trim_end_matches('/');
    if trailing_slashes {
        if fragment.is_empty() {
            format!("{base}/")
        } else {
            format!("{base}/{fragment}")
        }
    } else {
        format!("{base}{fragment}")
    }
}

fn join_spec_link(prefix: &str, spec_id: &str, fragment: &str, trailing_slashes: bool) -> String {
    join_prefix(prefix, build_spec_path(spec_id, fragment, trailing_slashes))
}

fn relative_prefix_for_spec_id(spec_id: &str) -> String {
    let depth = spec_id
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    if depth == 0 {
        "./".to_string()
    } else {
        "../".repeat(depth)
    }
}

fn format_display_id(prefix: &str, base_id: &str) -> String {
    if prefix.is_empty() {
        base_id.to_string()
    } else {
        format!("{prefix}{base_id}")
    }
}

struct LayoutAssets<'a> {
    css: &'a str,
    theme_init_js: &'a str,
    theme_toggle_js: &'a str,
    mermaid_js_url: Option<&'a str>,
    mermaid_init_js: Option<&'a str>,
}

fn format_generated_at(timestamp: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .unwrap_or_else(Local::now)
        .format("%Y-%m-%d %H:%M:%S %z")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn base_layout(
    site_name: &str,
    site_description: &str,
    title: &str,
    description: &str,
    assets: LayoutAssets,
    content: Markup,
    prefix: &str,
    generated_at: i64,
) -> Markup {
    let LayoutAssets {
        css,
        theme_init_js,
        theme_toggle_js,
        mermaid_js_url,
        mermaid_init_js,
    } = assets;
    let home_href = join_prefix(prefix, "");
    let favicon_href = join_prefix(prefix, "favicon.svg");
    let version = env!("CARGO_PKG_VERSION");
    let formatted_generated_at = format_generated_at(generated_at);
    html! {
        (PreEscaped("<!doctype html>"))
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                meta name="description" content=(description);
                link rel="icon" type="image/svg+xml" href=(favicon_href.clone());
                title { (title) }
                style { (PreEscaped(css)) }
                script { (PreEscaped(theme_init_js)) }
            }
            body {
                header class="site-header" {
                    div class="container" {
                        a href=(home_href.clone()) class="brand" {
                            img class="brand-mark" src=(favicon_href) alt="" role="presentation";
                            span class="brand-name" { (site_name) }
                        }
                        @if !site_description.is_empty() {
                            span class="tagline" { (site_description) }
                        }
                        button id="theme-toggle" type="button" class="theme-toggle" aria-label="Toggle light/dark mode" {
                            span class="sr-only" { "Switch color theme" }
                            svg class="theme-icon" viewBox="0 0 24 24" aria-hidden="true" {
                                path fill="currentColor" d="M12 3a9 9 0 1 0 0 18V3Z" {}
                                path d="M12 4a8 8 0 0 1 0 16" stroke="currentColor" stroke-width="2" stroke-linecap="round" {}
                            }
                        }
                    }
                }
                (content)
                footer class="site-footer" {
                    div class="container" {
                        "Powered by "
                        a href="https://dossie.rs" { "Dossiers" }
                        " v" (version)
                        span class="footer-label" { " • Built " (formatted_generated_at) }
                    }
                }
                @if let Some(mermaid_js_url) = mermaid_js_url {
                    script src=(mermaid_js_url) {}
                }
                @if let Some(mermaid_init_js) = mermaid_init_js {
                    script { (PreEscaped(mermaid_init_js)) }
                }
                script { (PreEscaped(theme_toggle_js)) }
            }
        }
    }
}

fn spec_from_generated(spec: GeneratedSpec) -> Result<SpecDocument> {
    let created = normalize_timestamp(spec.created.as_ref());
    let updated = normalize_timestamp(spec.updated.as_ref())
        .or(created)
        .or_else(|| normalize_timestamp(spec.updated_sort.as_ref()));

    let updated_sort = normalize_timestamp(spec.updated_sort.as_ref())
        .or(updated)
        .or(created)
        .unwrap_or(0);

    let format = match spec.format.to_lowercase().as_str() {
        "markdown" => DocFormat::Markdown,
        _ => DocFormat::Asciidoc,
    };

    let source = strip_frontmatter(&spec.source, &format);

    Ok(SpecDocument {
        id: spec.id,
        dir_name: spec.dir_name,
        title: spec.title,
        status: spec.status,
        created,
        updated,
        authors: normalize_authors(spec.authors),
        links: spec.links,
        updated_sort,
        extra: spec.extra,
        source,
        format,
        listed: true,
        revision_of: None,
        pr_number: None,
    })
}

#[derive(Clone, Copy)]
struct DocRenderer;

impl DocRenderer {
    fn new() -> Self {
        Self
    }

    fn render(&self, source: &str, format: DocFormat) -> Result<String, RenderError> {
        match format {
            DocFormat::Markdown => Ok(render_markdown(source)),
            DocFormat::Asciidoc => self.render_asciidoc(source),
        }
    }

    fn render_asciidoc(&self, source: &str) -> Result<String, RenderError> {
        let rendered = std::panic::catch_unwind(|| {
            let mut parser = AsciidocParser::default();
            let document = parser.parse(source);
            render_asciidoc_document(&document)
        })
        .map_err(|panic| {
            RenderError::Renderer(format!("asciidoc panic: {}", describe_panic(panic)))
        })?;

        Ok(rendered)
    }
}

fn render_spec_body(
    state: &AppState,
    spec: &SpecDocument,
    asset_base: String,
    link_prefix: &str,
    trailing_slashes: bool,
) -> Result<String, RenderError> {
    let rendered = match state.renderer.render(&spec.source, spec.format) {
        Ok(html) => html,
        Err(err) => {
            eprintln!(
                "Warning: failed to render spec {} as {:?}: {err}",
                spec.id, spec.format
            );
            render_plaintext(&spec.source)
        }
    };
    let without_heading = remove_leading_heading(&rendered);
    let prefixed_assets = prefix_asset_urls(&without_heading, &asset_base);
    let rewritten_links = rewrite_spec_links(
        &prefixed_assets,
        &state.spec_ids,
        link_prefix,
        trailing_slashes,
    );
    Ok(rewritten_links)
}

fn has_mermaid_markup(html: &str) -> bool {
    html.contains("class=\"mermaid\"")
        || html.contains("class='mermaid'")
        || html.contains("language-mermaid")
        || html.contains("lang-mermaid")
}

fn render_asciidoc_document(doc: &AsciidocDocument<'_>) -> String {
    let mut html = String::new();

    if let Some(title) = doc.header().title() {
        let attrs = build_attrs(None, &["adoc-doc-title"], &[]);
        let _ = write!(html, "<h1{attrs}>{title}</h1>");
    }

    render_asciidoc_blocks(doc.nested_blocks(), &mut html);
    html
}

fn render_asciidoc_blocks<'a>(
    blocks: impl IntoIterator<Item = &'a AsciidocBlock<'a>>,
    buf: &mut String,
) {
    for block in blocks {
        render_asciidoc_block(block, buf);
    }
}

fn render_asciidoc_block(block: &AsciidocBlock<'_>, buf: &mut String) {
    match block {
        AsciidocBlock::Simple(b) => render_simple_block(b, buf),
        AsciidocBlock::Media(b) => render_media_block(b, buf),
        AsciidocBlock::Section(b) => render_section_block(b, buf),
        AsciidocBlock::RawDelimited(b) => render_raw_block(b, buf),
        AsciidocBlock::CompoundDelimited(b) => render_compound_block(b, buf),
        AsciidocBlock::Preamble(b) => render_container(
            b.id(),
            &b.roles(),
            &["adoc-block", "preamble"],
            None,
            b.nested_blocks(),
            buf,
        ),
        AsciidocBlock::Break(b) => render_break_block(b, buf),
        AsciidocBlock::DocumentAttribute(_) => {}
        _ => {}
    }
}

fn render_simple_block(block: &SimpleBlock<'_>, buf: &mut String) {
    let roles = block.roles();
    let context = block.resolved_context();
    let classes = ["adoc-block", context.as_ref()];
    let attrs = build_attrs(block.id(), &classes, &roles);
    buf.push_str("<div");
    buf.push_str(&attrs);
    buf.push('>');
    render_block_title(block.title(), buf);

    match block.style() {
        SimpleBlockStyle::Paragraph => {
            let _ = write!(buf, "<p>{}</p>", block.content().rendered());
        }
        SimpleBlockStyle::Literal => {
            let _ = write!(
                buf,
                "<pre><code>{}</code></pre>",
                block.content().rendered()
            );
        }
        SimpleBlockStyle::Listing | SimpleBlockStyle::Source => {
            if is_mermaid_attrlist(block.attrlist()) {
                let _ = write!(
                    buf,
                    "<pre class=\"mermaid\">{}</pre>",
                    block.content().rendered()
                );
            } else if is_svgbob_attrlist(block.attrlist()) {
                let svg = render_svgbob_svg(block.content().rendered());
                let _ = write!(buf, "<div class=\"svgbob\">{svg}</div>");
            } else {
                let _ = write!(
                    buf,
                    "<pre><code>{}</code></pre>",
                    block.content().rendered()
                );
            }
        }
    }

    buf.push_str("</div>");
}

fn render_media_block(block: &MediaBlock<'_>, buf: &mut String) {
    let roles = block.roles();
    let context = block.resolved_context();
    let classes = ["adoc-block", context.as_ref()];
    let attrs = build_attrs(block.id(), &classes, &roles);
    buf.push_str("<figure");
    buf.push_str(&attrs);
    buf.push('>');
    render_block_title(block.title(), buf);

    let target = block.target().map(|t| t.data()).unwrap_or_default();
    let alt = block
        .macro_attrlist()
        .named_or_positional_attribute("alt", 1)
        .map(|attr| attr.value())
        .unwrap_or("");

    match block.type_() {
        MediaType::Image => {
            let _ = write!(
                buf,
                "<img src=\"{}\" alt=\"{}\" />",
                escape_attr(target),
                escape_attr(alt)
            );
        }
        MediaType::Video => {
            let _ = write!(
                buf,
                "<video controls src=\"{}\"></video>",
                escape_attr(target)
            );
        }
        MediaType::Audio => {
            let _ = write!(
                buf,
                "<audio controls src=\"{}\"></audio>",
                escape_attr(target)
            );
        }
    }

    buf.push_str("</figure>");
}

fn render_section_block(block: &SectionBlock<'_>, buf: &mut String) {
    let roles = block.roles();
    let attrs = build_attrs(block.id(), &["adoc-section"], &roles);
    buf.push_str("<section");
    buf.push_str(&attrs);
    buf.push('>');

    let heading_level = (block.level().saturating_add(1)).min(6);
    let mut heading_text = String::new();
    if let Some(number) = block.section_number() {
        let _ = write!(heading_text, "{number} ");
    }
    heading_text.push_str(block.section_title());

    let _ = write!(
        buf,
        "<h{level}>{text}</h{level}>",
        level = heading_level,
        text = heading_text
    );

    render_asciidoc_blocks(block.nested_blocks(), buf);

    buf.push_str("</section>");
}

fn render_raw_block(block: &RawDelimitedBlock<'_>, buf: &mut String) {
    let context = block.resolved_context();
    if context.as_ref() == "comment" {
        return;
    }

    let roles = block.roles();
    let classes = ["adoc-block", context.as_ref()];
    let attrs = build_attrs(block.id(), &classes, &roles);
    buf.push_str("<div");
    buf.push_str(&attrs);
    buf.push('>');
    render_block_title(block.title(), buf);

    match context.as_ref() {
        "pass" => buf.push_str(block.content().rendered()),
        "literal" => {
            if is_mermaid_attrlist(block.attrlist()) {
                let _ = write!(
                    buf,
                    "<pre class=\"mermaid\">{}</pre>",
                    block.content().rendered()
                );
            } else if is_svgbob_attrlist(block.attrlist()) {
                let svg = render_svgbob_svg(block.content().rendered());
                let _ = write!(buf, "<div class=\"svgbob\">{svg}</div>");
            } else {
                let _ = write!(
                    buf,
                    "<pre><code>{}</code></pre>",
                    block.content().rendered()
                );
            }
        }
        "listing" => {
            if is_mermaid_attrlist(block.attrlist()) {
                let _ = write!(
                    buf,
                    "<pre class=\"mermaid\">{}</pre>",
                    block.content().rendered()
                );
            } else if is_svgbob_attrlist(block.attrlist()) {
                let svg = render_svgbob_svg(block.content().rendered());
                let _ = write!(buf, "<div class=\"svgbob\">{svg}</div>");
            } else {
                let _ = write!(
                    buf,
                    "<pre><code>{}</code></pre>",
                    block.content().rendered()
                );
            }
        }
        _ => buf.push_str(block.content().rendered()),
    }

    buf.push_str("</div>");
}

fn render_compound_block(block: &CompoundDelimitedBlock<'_>, buf: &mut String) {
    let roles = block.roles();
    let context = block.resolved_context();
    let classes = ["adoc-block", context.as_ref()];
    let attrs = build_attrs(block.id(), &classes, &roles);

    buf.push_str("<div");
    buf.push_str(&attrs);
    buf.push('>');
    render_block_title(block.title(), buf);
    render_asciidoc_blocks(block.nested_blocks(), buf);
    buf.push_str("</div>");
}

fn render_break_block(block: &AsciidocBreak<'_>, buf: &mut String) {
    let roles = block.roles();
    let context = block.resolved_context();
    let classes = ["adoc-block", context.as_ref()];
    let attrs = build_attrs(block.id(), &classes, &roles);

    match block.type_() {
        BreakType::Thematic => {
            let _ = write!(buf, "<hr{attrs} />");
        }
        BreakType::Page => {
            buf.push_str("<div");
            buf.push_str(&attrs);
            buf.push_str("></div>");
        }
    }
}

fn render_container<'a>(
    id: Option<&'a str>,
    roles: &[&'a str],
    classes: &[&'a str],
    title: Option<&str>,
    blocks: impl IntoIterator<Item = &'a AsciidocBlock<'a>>,
    buf: &mut String,
) {
    let attrs = build_attrs(id, classes, roles);
    buf.push_str("<div");
    buf.push_str(&attrs);
    buf.push('>');
    render_block_title(title, buf);
    render_asciidoc_blocks(blocks, buf);
    buf.push_str("</div>");
}

fn render_block_title(title: Option<&str>, buf: &mut String) {
    if let Some(title) = title {
        let _ = write!(buf, "<div class=\"adoc-title\">{title}</div>");
    }
}

fn build_attrs(id: Option<&str>, classes: &[&str], roles: &[&str]) -> String {
    let mut all_classes = Vec::new();
    all_classes.extend(classes.iter().copied().filter(|c| !c.is_empty()));
    all_classes.extend(roles.iter().copied().filter(|r| !r.is_empty()));

    let mut attrs = String::new();
    if let Some(id) = id {
        attrs.push_str(" id=\"");
        attrs.push_str(&escape_attr(id));
        attrs.push('"');
    }

    if !all_classes.is_empty() {
        let value = all_classes.join(" ");
        if !value.is_empty() {
            attrs.push_str(" class=\"");
            attrs.push_str(&escape_attr(&value));
            attrs.push('"');
        }
    }

    attrs
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_mermaid_fence(info: &str) -> bool {
    let trimmed = info.trim();
    if trimmed.is_empty() {
        return false;
    }
    let token = trimmed
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
        .next()
        .unwrap_or("");
    token.eq_ignore_ascii_case("mermaid")
}

fn is_svgbob_fence(info: &str) -> bool {
    let trimmed = info.trim();
    if trimmed.is_empty() {
        return false;
    }
    let token = trimmed
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
        .next()
        .unwrap_or("");
    token.eq_ignore_ascii_case("svgbob")
}

fn is_mermaid_attrlist(attrlist: Option<&Attrlist<'_>>) -> bool {
    let Some(attrlist) = attrlist else {
        return false;
    };

    if let Some(style) = attrlist.block_style() {
        if style.eq_ignore_ascii_case("mermaid") {
            return true;
        }
        if style.eq_ignore_ascii_case("source") || style.eq_ignore_ascii_case("listing") {
            if let Some(lang) = attrlist.nth_attribute(2).map(|attr| attr.value()) {
                if lang.eq_ignore_ascii_case("mermaid") {
                    return true;
                }
            }
        }
    }

    if let Some(lang) = attrlist
        .named_attribute("language")
        .or_else(|| attrlist.named_attribute("source-language"))
        .map(|attr| attr.value())
    {
        if lang.eq_ignore_ascii_case("mermaid") {
            return true;
        }
    }

    false
}

fn is_svgbob_attrlist(attrlist: Option<&Attrlist<'_>>) -> bool {
    let Some(attrlist) = attrlist else {
        return false;
    };

    if let Some(style) = attrlist.block_style() {
        if style.eq_ignore_ascii_case("svgbob") {
            return true;
        }
        if style.eq_ignore_ascii_case("source") || style.eq_ignore_ascii_case("listing") {
            if let Some(lang) = attrlist.nth_attribute(2).map(|attr| attr.value()) {
                if lang.eq_ignore_ascii_case("svgbob") {
                    return true;
                }
            }
        }
    }

    if let Some(lang) = attrlist
        .named_attribute("language")
        .or_else(|| attrlist.named_attribute("source-language"))
        .map(|attr| attr.value())
    {
        if lang.eq_ignore_ascii_case("svgbob") {
            return true;
        }
    }

    false
}

#[derive(Clone, Copy)]
enum SpecialCodeBlock {
    Mermaid,
    Svgbob,
}

fn render_svgbob_svg(source: &str) -> String {
    let settings = svgbob::Settings {
        background: "var(--svgbob-background, transparent)".to_string(),
        stroke_color: "var(--svgbob-stroke, var(--text))".to_string(),
        fill_color: "var(--svgbob-fill, var(--text))".to_string(),
        include_backdrop: false,
        ..Default::default()
    };
    svgbob::to_svg_with_settings(source, &settings)
}

fn render_plaintext(source: &str) -> String {
    format!("<pre>{}</pre>", escape_html(source))
}

fn describe_panic(panic: Box<dyn Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn render_markdown(source: &str) -> String {
    let mut options = MdOptions::empty();
    options.insert(MdOptions::ENABLE_TABLES);
    options.insert(MdOptions::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(source, options);
    let special_block = Cell::new(None);
    let svgbob_buffer = RefCell::new(String::new());
    let parser = parser.map(|event| match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
            if is_mermaid_fence(&info) {
                special_block.set(Some(SpecialCodeBlock::Mermaid));
                Event::Html("<pre class=\"mermaid\">".into())
            } else if is_svgbob_fence(&info) {
                special_block.set(Some(SpecialCodeBlock::Svgbob));
                svgbob_buffer.borrow_mut().clear();
                Event::Html("".into())
            } else {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
            }
        }
        Event::End(TagEnd::CodeBlock) => match special_block.get() {
            Some(SpecialCodeBlock::Mermaid) => {
                special_block.set(None);
                Event::Html("</pre>".into())
            }
            Some(SpecialCodeBlock::Svgbob) => {
                special_block.set(None);
                let source = svgbob_buffer.borrow().clone();
                let svg = render_svgbob_svg(&source);
                Event::Html(format!("<div class=\"svgbob\">{svg}</div>").into())
            }
            None => Event::End(TagEnd::CodeBlock),
        },
        Event::Text(text) => match special_block.get() {
            Some(SpecialCodeBlock::Svgbob) => {
                svgbob_buffer.borrow_mut().push_str(&text);
                Event::Html("".into())
            }
            _ => Event::Text(text),
        },
        Event::SoftBreak | Event::HardBreak => match special_block.get() {
            Some(SpecialCodeBlock::Svgbob) => {
                svgbob_buffer.borrow_mut().push('\n');
                Event::Html("".into())
            }
            _ => event,
        },
        _ => event,
    });
    let mut html = String::new();
    md_html::push_html(&mut html, parser);
    html
}

fn remove_leading_heading(html: &str) -> String {
    lazy_static! {
        static ref HEADING_RE: Regex = Regex::new(r"(?is)^\s*<h1[^>]*>.*?</h1>\s*").unwrap();
    }
    HEADING_RE.replace(html, "").to_string()
}

fn prefix_asset_urls(html: &str, asset_base: &str) -> String {
    lazy_static! {
        static ref ASSET_RE: Regex =
            Regex::new(r#"(?i)\b(src|href)=(\"([^\"]+)\"|'([^']+)')"#).unwrap();
        static ref SCHEME_RE: Regex = Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*:").unwrap();
        static ref SPEC_RE: Regex = Regex::new(
            r#"(?i)^(?:\.\./)+(?:specs/)?(\d{4,})-[^/]+/[^#?]*?(?:\.adoc|\.md)?(#[-A-Za-z0-9_]+)?$"#
        )
        .unwrap();
    }

    let base = if asset_base.is_empty() || asset_base.ends_with('/') {
        asset_base.to_string()
    } else {
        format!("{asset_base}/")
    };

    ASSET_RE
        .replace_all(html, |caps: &regex::Captures| {
            let original = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let attr = &caps[1];
            let value = caps
                .get(3)
                .or_else(|| caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");
            let quote = if caps.get(3).is_some() { "\"" } else { "'" };
            let attr_lower = attr.to_ascii_lowercase();
            if value.is_empty()
                || value.starts_with('#')
                || value.starts_with('/')
                || value.starts_with("//")
                || SCHEME_RE.is_match(value)
                || (attr_lower == "href" && SPEC_RE.is_match(value))
            {
                return original.to_string();
            }

            let mut value = value.trim_start_matches("./").trim_start_matches('/');
            while value.starts_with("../") {
                value = value.trim_start_matches("../");
            }
            format!(r#"{attr}={quote}{base}{value}{quote}"#)
        })
        .to_string()
}

fn rewrite_spec_links(
    html: &str,
    spec_ids: &HashSet<String>,
    prefix: &str,
    trailing_slashes: bool,
) -> String {
    lazy_static! {
        static ref HREF_RE: Regex = Regex::new(r#"(?i)\bhref=(\"([^\"]+)\"|'([^']+)')"#).unwrap();
    }

    HREF_RE
        .replace_all(html, |caps: &regex::Captures| {
            let url = caps
                .get(2)
                .or_else(|| caps.get(3))
                .map(|m| m.as_str())
                .unwrap_or("");
            let quote = if caps.get(2).is_some() { "\"" } else { "'" };
            let rewritten = normalize_spec_link(url, spec_ids, prefix, trailing_slashes);
            format!(r#"href={quote}{rewritten}{quote}"#)
        })
        .to_string()
}

fn normalize_spec_link(
    url: &str,
    spec_ids: &HashSet<String>,
    prefix: &str,
    trailing_slashes: bool,
) -> String {
    if url.is_empty() || url.starts_with('#') {
        return url.to_string();
    }

    lazy_static! {
        static ref SCHEME_RE: Regex = Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*:").unwrap();
        static ref SPEC_RE: Regex = Regex::new(
            r#"(?i)^(?:\.\./)+(?:specs/)?(\d{4,})-[^/]+/[^#?]*?(?:\.adoc|\.md)?(#[-A-Za-z0-9_]+)?$"#
        )
        .unwrap();
    }

    if SCHEME_RE.is_match(url) {
        return url.to_string();
    }

    if let Some(caps) = SPEC_RE.captures(url) {
        let spec_id = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let fragment = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        if spec_ids.contains(&spec_id) {
            return join_spec_link(prefix, &spec_id, fragment, trailing_slashes);
        }
    }

    url.to_string()
}

fn normalize_timestamp(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(num)) => num
            .as_i64()
            .or_else(|| num.as_f64().map(|v| v as i64))
            .filter(|v| *v > 0),
        Some(Value::String(text)) => parse_date(text),
        _ => None,
    }
}

fn parse_date(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    parse_numeric_date(trimmed)
        .or_else(|| parse_named_month_date(trimmed))
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .ok()
                .map(|dt| dt.timestamp_millis())
        })
        .or_else(|| {
            chrono::DateTime::parse_from_rfc2822(trimmed)
                .ok()
                .map(|dt| dt.timestamp_millis())
        })
}

fn extract_leading_title(source: &str, format: &DocFormat) -> Option<String> {
    match format {
        DocFormat::Asciidoc => extract_asciidoc_leading_title(source),
        DocFormat::Markdown => extract_markdown_leading_h1(source),
    }
}

fn extract_markdown_leading_h1(source: &str) -> Option<String> {
    let mut lines = source.lines().peekable();
    let mut in_comment = false;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if in_comment {
            if trimmed.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("<!--") {
            in_comment = !trimmed.contains("-->");
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }

        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with('#') {
            let hashes = trimmed_start.chars().take_while(|c| *c == '#').count();
            if hashes >= 1 {
                let title = trimmed_start.trim_start_matches('#').trim();
                if !title.is_empty() {
                    return Some(title.to_string());
                }
            }
            return None;
        }

        if let Some(underline) = lines.peek() {
            let underline_trimmed = underline.trim();
            if !underline_trimmed.is_empty() && underline_trimmed.chars().all(|c| c == '=') {
                lines.next();
                return Some(trimmed.to_string()).filter(|title| !title.is_empty());
            }
        }

        return None;
    }

    None
}

fn extract_asciidoc_leading_title(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }

        let leading_equals = trimmed.chars().take_while(|c| *c == '=').count();
        if leading_equals >= 1 {
            let title = trimmed.trim_start_matches('=').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }

        return None;
    }

    None
}

fn extract_spec_id(name: &str) -> Option<String> {
    lazy_static! {
        static ref ID_RE: Regex = Regex::new(r"^(\d{4,})").unwrap();
    }
    ID_RE
        .captures(name)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn display_name_from_dir(dir_name: &str) -> String {
    lazy_static! {
        static ref NAME_PREFIX_RE: Regex = Regex::new(r"^\d{4,}-").unwrap();
    }

    let cleaned = NAME_PREFIX_RE
        .replace(dir_name, "")
        .trim_end_matches(".md")
        .trim_end_matches(".markdown")
        .trim_end_matches(".adoc")
        .trim_end_matches(".asciidoc")
        .to_string();
    if cleaned.is_empty() {
        dir_name.to_string()
    } else {
        cleaned
    }
}

fn find_doc_file(dir: &Path) -> Result<(PathBuf, DocFormat)> {
    let mut asciidoc_files = Vec::new();
    let mut markdown_files = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();

        match ext.as_str() {
            "adoc" | "asciidoc" => asciidoc_files.push(path),
            "md" | "markdown" => markdown_files.push(path),
            _ => {}
        }
    }

    if let Some(path) = markdown_files.into_iter().min() {
        return Ok((path, DocFormat::Markdown));
    }
    if let Some(path) = asciidoc_files.into_iter().min() {
        return Ok((path, DocFormat::Asciidoc));
    }

    bail!(
        "No spec document found in {} (expected .md/.markdown or .adoc/.asciidoc file)",
        dir.display()
    );
}

fn parse_numeric_date(input: &str) -> Option<i64> {
    lazy_static! {
        static ref NUMERIC_RE: Regex = Regex::new(
            r"(?xi)^\(?(\d{4})-(\d{2})-(\d{2})\)?(?:\s+(\d{1,2}):(\d{2})(?::(\d{2}))?)?$|^(\d{1,4})[/. -](\d{1,2})[/. -](\d{1,4})(?:\s+(\d{1,2}):(\d{2})(?::(\d{2}))?)?$"
        )
        .unwrap();
    }

    let input_trimmed = input.trim();
    let caps = NUMERIC_RE.captures(input_trimmed)?;

    // ISO yyyy-mm-dd (with optional parentheses) possibly with time
    if let (Some(y), Some(m), Some(d)) = (caps.get(1), caps.get(2), caps.get(3)) {
        let hour = caps.get(4).map(|m| m.as_str());
        let minute = caps.get(5).map(|m| m.as_str());
        let second_part = caps.get(6).map(|m| m.as_str());
        let time = parse_time_parts(hour, minute, second_part);
        return build_utc_timestamp(
            y.as_str().parse().ok()?,
            m.as_str().parse().ok()?,
            d.as_str().parse().ok()?,
            time,
        );
    }

    let first = caps.get(7)?.as_str();
    let second = caps.get(8)?.as_str();
    let third = caps.get(9)?.as_str();
    let hour = caps.get(10).map(|m| m.as_str());
    let minute = caps.get(11).map(|m| m.as_str());
    let second_part = caps.get(12).map(|m| m.as_str());

    let time = parse_time_parts(hour, minute, second_part);

    if first.len() == 4 {
        return build_utc_timestamp(
            first.parse().ok()?,
            second.parse().ok()?,
            third.parse().ok()?,
            time,
        );
    }

    if third.len() == 4 {
        let first_num: i32 = first.parse().ok()?;
        let second_num: i32 = second.parse().ok()?;
        let year: i32 = third.parse().ok()?;

        if first_num > 12 {
            return build_utc_timestamp(year, second_num, first_num, time);
        }

        if second_num > 12 {
            return build_utc_timestamp(year, first_num, second_num, time);
        }

        return build_utc_timestamp(year, second_num, first_num, time);
    }

    None
}

fn parse_named_month_date(input: &str) -> Option<i64> {
    lazy_static! {
        static ref MONTH_FIRST_RE: Regex = Regex::new(
            r"(?iu)^([\p{L}.]+)\s+(\d{1,2})(?:,)?\s+(\d{4})(?:\s+(\d{1,2}):(\d{2})(?::(\d{2}))?)?$"
        )
        .unwrap();
        static ref DAY_FIRST_RE: Regex = Regex::new(
            r"(?iu)^(\d{1,2})\s+([\p{L}.]+)\s+(\d{4})(?:\s+(\d{1,2}):(\d{2})(?::(\d{2}))?)?$"
        )
        .unwrap();
    }

    if let Some(caps) = MONTH_FIRST_RE.captures(input) {
        let month = lookup_month_index(caps.get(1)?.as_str())?;
        let day: i32 = caps.get(2)?.as_str().parse().ok()?;
        let year: i32 = caps.get(3)?.as_str().parse().ok()?;
        let time = parse_time_parts(
            caps.get(4).map(|m| m.as_str()),
            caps.get(5).map(|m| m.as_str()),
            caps.get(6).map(|m| m.as_str()),
        );
        return build_utc_timestamp(year, month, day, time);
    }

    if let Some(caps) = DAY_FIRST_RE.captures(input) {
        let day: i32 = caps.get(1)?.as_str().parse().ok()?;
        let month = lookup_month_index(caps.get(2)?.as_str())?;
        let year: i32 = caps.get(3)?.as_str().parse().ok()?;
        let time = parse_time_parts(
            caps.get(4).map(|m| m.as_str()),
            caps.get(5).map(|m| m.as_str()),
            caps.get(6).map(|m| m.as_str()),
        );
        return build_utc_timestamp(year, month, day, time);
    }

    None
}

fn lookup_month_index(raw: &str) -> Option<i32> {
    lazy_static! {
        static ref MONTHS: HashMap<String, i32> = HashMap::from([
            ("january".into(), 1),
            ("jan".into(), 1),
            ("januar".into(), 1),
            ("february".into(), 2),
            ("feb".into(), 2),
            ("februar".into(), 2),
            ("march".into(), 3),
            ("mar".into(), 3),
            ("marz".into(), 3),
            ("maerz".into(), 3),
            ("april".into(), 4),
            ("apr".into(), 4),
            ("may".into(), 5),
            ("mai".into(), 5),
            ("june".into(), 6),
            ("jun".into(), 6),
            ("juni".into(), 6),
            ("july".into(), 7),
            ("jul".into(), 7),
            ("juli".into(), 7),
            ("august".into(), 8),
            ("aug".into(), 8),
            ("september".into(), 9),
            ("sep".into(), 9),
            ("sept".into(), 9),
            ("october".into(), 10),
            ("oct".into(), 10),
            ("oktober".into(), 10),
            ("okt".into(), 10),
            ("november".into(), 11),
            ("nov".into(), 11),
            ("december".into(), 12),
            ("dec".into(), 12),
            ("dezember".into(), 12),
            ("dez".into(), 12),
        ]);
    }

    let normalized = raw
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        .replace('.', "");

    MONTHS.get(&normalized).copied()
}

fn parse_time_parts(
    hour: Option<&str>,
    minute: Option<&str>,
    second: Option<&str>,
) -> (u32, u32, u32) {
    let h = hour.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let m = minute.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let s = second.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    (h, m, s)
}

fn build_utc_timestamp(year: i32, month: i32, day: i32, (h, m, s): (u32, u32, u32)) -> Option<i64> {
    let date = NaiveDate::from_ymd_opt(year, month as u32, day.try_into().ok()?)?;
    let dt = date.and_hms_opt(h, m, s)?;
    Some(dt.and_utc().timestamp_millis())
}

fn format_spec_date(timestamp: Option<i64>, include_time: bool) -> Option<String> {
    let ts = timestamp?;
    let dt = Local.timestamp_millis_opt(ts).single()?;
    if include_time {
        Some(dt.format("%b %-d %Y, %-I:%M %p").to_string())
    } else {
        Some(dt.format("%b %-d %Y").to_string())
    }
}

fn format_iso_date(timestamp: Option<i64>) -> Option<String> {
    let ts = timestamp?;
    let dt = Local.timestamp_millis_opt(ts).single()?;
    Some(dt.format("%Y-%m-%d").to_string())
}

fn slugify_author(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in name.nfkc() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                slug.push(lower);
                last_dash = false;
            }
            continue;
        }

        if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn slugify_status(status: &str) -> String {
    slugify_author(status)
}

fn collect_status_summaries(specs: &[SpecDocument]) -> Vec<StatusSummary> {
    let mut summaries: HashMap<String, StatusSummary> = HashMap::new();
    for spec in specs.iter().filter(|spec| spec.listed) {
        let slug = slugify_status(&spec.status);
        let entry = summaries
            .entry(slug.clone())
            .or_insert_with(|| StatusSummary {
                name: spec.status.clone(),
                slug,
                count: 0,
            });
        entry.count += 1;
    }

    let mut collected: Vec<StatusSummary> = summaries.into_values().collect();
    collected.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    collected
}

fn resolve_site_name(_project_root: &Path, project_config: &ProjectConfiguration) -> String {
    if let Ok(env_name) = std::env::var("SITE_NAME") {
        let trimmed = env_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Some(title) = project_config.title.clone() {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    "Dossiers".into()
}

fn resolve_config_path(project_root: &Path, override_path: Option<&Path>) -> Option<PathBuf> {
    override_path.map(|p| p.to_path_buf()).or_else(|| {
        let default = project_root.join("dossiers.toml");
        default.exists().then_some(default)
    })
}

fn load_project_configuration(
    project_root: &Path,
    override_path: Option<&Path>,
) -> ProjectConfiguration {
    let config_path = resolve_config_path(project_root, override_path);

    let Some(path) = config_path else {
        return ProjectConfiguration::default();
    };

    let raw = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "Warning: failed to read project configuration at {}: {err}",
                path.display()
            );
            return ProjectConfiguration::default();
        }
    };

    match parse_toml_config(&raw, &path) {
        Ok(value) => ProjectConfiguration::from_json_value(&value),
        Err(err) => {
            eprintln!(
                "Warning: failed to parse project configuration at {}: {err}",
                path.display()
            );
            ProjectConfiguration::default()
        }
    }
}

fn parse_toml_config(raw: &str, path: &Path) -> Result<Value, String> {
    toml::from_str::<toml::Value>(raw)
        .map_err(|err| format!("TOML parse error: {err}"))
        .and_then(|value| serde_json::to_value(value).map_err(|err| err.to_string()))
        .map_err(|err| format!("failed to parse config {}: {err}", path.display()))
}

fn validate_project_config_for_check(
    project_root: &Path,
    override_path: Option<&Path>,
) -> (ProjectConfiguration, CategoryReport) {
    let mut report = CategoryReport::default();
    let config_path = resolve_config_path(project_root, override_path);
    let Some(path) = config_path else {
        return (ProjectConfiguration::default(), report);
    };

    let raw = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            report.add_error(
                format!(
                    "Failed to read project configuration at {}: {err}",
                    path.display()
                ),
                None,
            );
            return (ProjectConfiguration::default(), report);
        }
    };

    let value = match parse_toml_config(&raw, &path) {
        Ok(value) => value,
        Err(err) => {
            report.add_error(
                format!("Failed to parse configuration at {}: {err}", path.display()),
                None,
            );
            return (ProjectConfiguration::default(), report);
        }
    };

    let config = ProjectConfiguration::from_json_value(&value);
    validate_config_value(&value, &config, &mut report, &path);

    (config, report)
}

fn validate_config_value(
    raw_value: &Value,
    config: &ProjectConfiguration,
    report: &mut CategoryReport,
    path: &Path,
) {
    if let Some(statuses) = raw_value
        .get("statuses")
        .or_else(|| raw_value.get("statusList"))
        .and_then(Value::as_array)
    {
        for (idx, status) in statuses.iter().enumerate() {
            let valid = status
                .as_str()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !valid {
                report.add_error(
                    format!("Status entry #{idx} must be a non-empty string"),
                    Some(path.display().to_string()),
                );
            }
        }
    }

    if let Some(default_status) = config.default_status.as_ref() {
        if !config.statuses.is_empty() && !config.statuses.contains(default_status) {
            report.add_error(
                format!(
                    "default_status '{}' is not included in statuses",
                    default_status
                ),
                Some(path.display().to_string()),
            );
        }
    }

    if let Some(fields) = raw_value
        .get("extraMetadataFields")
        .or_else(|| raw_value.get("extra_metadata_fields"))
        .and_then(Value::as_array)
    {
        for (idx, value) in fields.iter().enumerate() {
            let Some(map) = value.as_object() else {
                report.add_error(
                    format!("extra_metadata_fields[{idx}] must be a table"),
                    Some(path.display().to_string()),
                );
                continue;
            };

            let name = map
                .get("name")
                .and_then(Value::as_str)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            if name.is_none() {
                report.add_error(
                    format!("extra_metadata_fields[{idx}] is missing a non-empty name"),
                    Some(path.display().to_string()),
                );
            }

            let type_hint = map
                .get("type")
                .or_else(|| map.get("type_hint"))
                .or_else(|| map.get("typeHint"))
                .and_then(Value::as_str);

            match type_hint {
                Some(raw) if is_valid_metadata_type(raw) => {}
                Some(raw) => report.add_error(
                    format!("extra_metadata_fields[{idx}] has invalid type '{}'", raw),
                    Some(path.display().to_string()),
                ),
                None => report.add_error(
                    format!("extra_metadata_fields[{idx}] is missing a type"),
                    Some(path.display().to_string()),
                ),
            }
        }
    }

    for (alias, target) in &config.field_aliases {
        let canonical = canonicalize_field_key(alias);
        if matches!(
            canonical.as_str(),
            "title" | "status" | "created" | "updated" | "lastupdated"
        ) {
            report.add_warning(
                format!(
                    "Field alias '{}' for '{}' conflicts with a standard field name",
                    alias, target
                ),
                Some(path.display().to_string()),
            );
        }
    }
}

fn canonicalize_field_key(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn is_valid_metadata_type(raw: &str) -> bool {
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "string" | "number" | "boolean" | "date" | "markdown"
    )
}

fn discover_specs_for_check(input_root: &Path) -> (Vec<LintSpec>, CategoryReport) {
    let mut report = CategoryReport::default();
    let mut specs = Vec::new();

    if !input_root.exists() {
        report.add_error(
            format!(
                "Specification directory not found: {}",
                input_root.display()
            ),
            None,
        );
        return (specs, report);
    }

    if !input_root.is_dir() {
        report.add_error(
            format!("Provided path is not a directory: {}", input_root.display()),
            None,
        );
        return (specs, report);
    }

    let mut candidates: HashMap<String, Vec<(String, PathBuf, DocFormat, PathBuf)>> =
        HashMap::new();

    let entries = match fs::read_dir(input_root) {
        Ok(entries) => entries,
        Err(err) => {
            report.add_error(
                format!(
                    "Failed to read specification directory {}: {err}",
                    input_root.display()
                ),
                None,
            );
            return (specs, report);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(id) = extract_spec_id(&name) else {
            continue;
        };

        if path.is_dir() {
            match find_doc_file(&path) {
                Ok((doc_path, format)) => {
                    candidates.entry(id).or_default().push((
                        name.clone(),
                        doc_path,
                        format,
                        path.clone(),
                    ));
                }
                Err(_) => {
                    report.add_error(
                        format!(
                            "Directory {} matches the expected pattern but has no document",
                            path.display()
                        ),
                        None,
                    );
                }
            }
            continue;
        }

        if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            let format = match ext.as_str() {
                "md" | "markdown" => Some(DocFormat::Markdown),
                "adoc" | "asciidoc" => Some(DocFormat::Asciidoc),
                _ => None,
            };

            if let Some(format) = format {
                let dir_name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(&name)
                    .to_string();
                let spec_root = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| input_root.to_path_buf());

                candidates
                    .entry(id)
                    .or_default()
                    .push((dir_name, path.clone(), format, spec_root));
            }
        }
    }

    if candidates.is_empty() {
        report.add_error(
            format!(
                "No spec documents found in {} (expected subdirectories like 0001-* or files like 0001-*.md)",
                input_root.display()
            ),
            None,
        );
        return (specs, report);
    }

    for (spec_id, entries) in &candidates {
        if entries.len() > 1 {
            let paths = entries
                .iter()
                .map(|(_, path, _, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            report.add_error(
                format!("Duplicate specification ID {} in {}", spec_id, paths),
                None,
            );
        }

        for (dir_name, doc_path, format, spec_root) in entries {
            match fs::read_to_string(doc_path) {
                Ok(source) => {
                    specs.push(LintSpec {
                        id: spec_id.clone(),
                        dir_name: dir_name.clone(),
                        doc_path: doc_path.clone(),
                        spec_root: spec_root.clone(),
                        source,
                        format: *format,
                    });
                }
                Err(err) => {
                    report.add_error(
                        format!("Failed to read {}: {err}", doc_path.display()),
                        None,
                    );
                }
            }
        }
    }

    (specs, report)
}

fn validate_metadata_for_specs(
    specs: &[LintSpec],
    project_config: &ProjectConfiguration,
) -> CategoryReport {
    let mut report = CategoryReport::default();
    if specs.is_empty() {
        return report;
    }

    let reader = MetadataReader::new(project_config.clone());

    for spec in specs {
        if matches!(spec.format, DocFormat::Markdown) {
            if let Some(issue) = detect_frontmatter_issue(&spec.source) {
                report.add_error(
                    format!("Malformed frontmatter in {}", spec.doc_path.display()),
                    Some(issue),
                );
            }
        }

        let display_name = display_name_from_dir(&spec.dir_name);
        let parsed = reader.read(&spec.source, spec.format, &display_name);
        let metadata = parsed.metadata;

        let missing_title = metadata
            .title
            .as_ref()
            .map(|t| t.trim().is_empty())
            .unwrap_or(true);
        if missing_title {
            report.add_error(
                format!("Missing title in {}", spec.doc_path.display()),
                None,
            );
        }

        if !project_config.statuses.is_empty() {
            let resolved_status = reader.resolve_status(metadata.status.clone(), false);
            if !resolved_status.is_empty() && !project_config.statuses.contains(&resolved_status) {
                report.add_error(
                    format!(
                        "Invalid status '{}' in {} (allowed: {})",
                        resolved_status,
                        spec.doc_path.display(),
                        project_config.statuses.join(", ")
                    ),
                    None,
                );
            }
        }

        for (field, raw) in [
            ("created", metadata.created.as_deref()),
            ("updated", metadata.updated.as_deref()),
        ] {
            if let Some(value) = raw {
                if parse_date(value).is_none() {
                    report.add_warning(
                        format!(
                            "Unparseable date '{}' for field '{}' in {}",
                            value,
                            field,
                            spec.doc_path.display()
                        ),
                        None,
                    );
                }
            }
        }
    }

    report
}

fn collect_spec_lint_data(specs: &[LintSpec]) -> HashMap<String, SpecLintData> {
    let mut data = HashMap::new();
    let renderer = DocRenderer::new();

    for spec in specs {
        let (assets, cross_refs) = collect_references(&spec.source, spec.format);
        let anchors = extract_heading_ids(&renderer, &spec.source, spec.format);
        data.insert(
            spec.id.clone(),
            SpecLintData {
                anchors,
                assets,
                cross_refs,
            },
        );
    }

    data
}

fn validate_asset_references(
    specs: &[LintSpec],
    lint_data: &HashMap<String, SpecLintData>,
) -> CategoryReport {
    let mut report = CategoryReport::default();

    for spec in specs {
        let Some(data) = lint_data.get(&spec.id) else {
            continue;
        };

        for asset in &data.assets {
            let (path_only, _) = split_target(&asset.path);
            if path_only.is_empty() {
                continue;
            }

            let resolved = spec.spec_root.join(&path_only);
            if !resolved.exists() {
                report.add_error(
                    format!("Missing asset: {}", path_only),
                    Some(format!(
                        "Referenced in: {}:{}",
                        spec.doc_path.display(),
                        asset.line
                    )),
                );
            }
        }
    }

    report
}

fn validate_cross_references(
    specs: &[LintSpec],
    lint_data: &HashMap<String, SpecLintData>,
) -> CategoryReport {
    let mut report = CategoryReport::default();
    let available: HashSet<String> = specs.iter().map(|spec| spec.id.clone()).collect();
    let empty: Vec<String> = Vec::new();

    for spec in specs {
        let Some(data) = lint_data.get(&spec.id) else {
            continue;
        };

        for reference in &data.cross_refs {
            if !available.contains(&reference.target_spec) {
                report.add_error(
                    format!("Broken reference: {}", reference.raw),
                    Some(format!(
                        "Referenced in: {}:{}",
                        spec.doc_path.display(),
                        reference.line
                    )),
                );
                continue;
            }

            if let Some(anchor) = reference.anchor.as_ref() {
                let anchors = lint_data
                    .get(&reference.target_spec)
                    .map(|d| &d.anchors)
                    .unwrap_or(&empty);

                if !anchors.iter().any(|candidate| candidate == anchor) {
                    report.add_error(
                        format!("Broken anchor: {}", reference.raw),
                        Some(format!(
                            "Referenced in: {}:{}",
                            spec.doc_path.display(),
                            reference.line
                        )),
                    );
                }
            }
        }
    }

    report
}

fn collect_references(
    source: &str,
    format: DocFormat,
) -> (Vec<AssetReference>, Vec<CrossReference>) {
    let raw_refs = match format {
        DocFormat::Markdown => collect_markdown_references(source),
        DocFormat::Asciidoc => collect_asciidoc_references(source),
    };

    let mut assets = Vec::new();
    let mut cross_refs = Vec::new();

    for (target, line) in raw_refs {
        match classify_reference(&target) {
            Some(ReferenceKind::Asset(path)) => assets.push(AssetReference { path, line }),
            Some(ReferenceKind::Cross {
                raw,
                target_spec,
                anchor,
            }) => cross_refs.push(CrossReference {
                raw,
                target_spec,
                anchor,
                line,
            }),
            None => {}
        }
    }

    (assets, cross_refs)
}

fn collect_markdown_references(source: &str) -> Vec<(String, usize)> {
    let mut options = MdOptions::empty();
    options.insert(MdOptions::ENABLE_TABLES);
    options.insert(MdOptions::ENABLE_FOOTNOTES);

    Parser::new_ext(source, options)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            pulldown_cmark::Event::Start(Tag::Link { dest_url, .. })
            | pulldown_cmark::Event::Start(Tag::Image { dest_url, .. }) => Some((
                dest_url.to_string(),
                line_number_from_offset(source, range.start),
            )),
            _ => None,
        })
        .collect()
}

fn collect_asciidoc_references(source: &str) -> Vec<(String, usize)> {
    lazy_static! {
        static ref ASCIIDOC_REF_RE: Regex =
            Regex::new(r"(?m)(?:image|link|xref)::?([^\s\[]+)\[").unwrap();
    }

    ASCIIDOC_REF_RE
        .captures_iter(source)
        .filter_map(|caps| {
            let m = caps.get(1)?;
            let target = m.as_str().to_string();
            let line = line_number_from_offset(source, m.start());
            Some((target, line))
        })
        .collect()
}

enum ReferenceKind {
    Asset(String),
    Cross {
        raw: String,
        target_spec: String,
        anchor: Option<String>,
    },
}

fn classify_reference(target: &str) -> Option<ReferenceKind> {
    let trimmed = target.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    if is_external_target(trimmed) || trimmed.starts_with('/') {
        return None;
    }

    lazy_static! {
        static ref SPEC_REF_RE: Regex = Regex::new(
            r#"(?i)^(?:\.\./)+(?:specs/)?(\d{4,})-[^/]+/[^#?]*?(?:\.adoc|\.md)?(#[-A-Za-z0-9_]+)?$"#
        )
        .unwrap();
    }

    if let Some(caps) = SPEC_REF_RE.captures(trimmed) {
        let spec_id = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let anchor = caps
            .get(2)
            .map(|m| m.as_str().trim_start_matches('#').to_string())
            .filter(|s| !s.is_empty());

        return Some(ReferenceKind::Cross {
            raw: trimmed.to_string(),
            target_spec: spec_id,
            anchor,
        });
    }

    let (path, _) = split_target(trimmed);
    if path.is_empty() {
        None
    } else {
        Some(ReferenceKind::Asset(path))
    }
}

fn split_target(raw: &str) -> (String, Option<String>) {
    let mut parts = raw.split('#');
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let anchor = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (path, anchor)
}

fn is_external_target(target: &str) -> bool {
    lazy_static! {
        static ref SCHEME_RE: Regex = Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*:").unwrap();
    }

    target.starts_with("//") || SCHEME_RE.is_match(target)
}

fn extract_heading_ids(renderer: &DocRenderer, source: &str, format: DocFormat) -> Vec<String> {
    lazy_static! {
        static ref HEADING_RE: Regex =
            Regex::new(r"(?is)<h([1-6])([^>]*)>(.*?)</h([1-6])\s*>").unwrap();
        static ref ID_RE: Regex = Regex::new(r#"id\s*=\s*["']([^"']+)["']"#).unwrap();
    }

    let rendered = renderer
        .render(source, format)
        .unwrap_or_else(|_| render_plaintext(source));
    let cleaned = remove_leading_heading(&rendered);
    let mut anchors = Vec::new();
    let mut used: HashSet<String> = HashSet::new();

    for caps in HEADING_RE.captures_iter(&cleaned) {
        let level: usize = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let closing_level: usize = caps
            .get(4)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(level);
        if level != closing_level || !(2..=4).contains(&level) {
            continue;
        }

        let attrs = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let text = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let base = slugify_heading_id(&strip_html_tags(text));
        let mut candidate = ID_RE
            .captures(attrs)
            .and_then(|m| m.get(1).map(|m| m.as_str().trim().to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| base.clone());

        let mut index = 2;
        while candidate.is_empty() || used.contains(&candidate) {
            candidate = format!("{base}-{index}");
            index += 1;
        }

        used.insert(candidate.clone());
        anchors.push(candidate);
    }

    anchors
}

fn slugify_heading_id(text: &str) -> String {
    lazy_static! {
        static ref SLUG_RE: Regex = Regex::new(r"[^\w]+").unwrap();
    }
    let lowered = text.to_lowercase().trim().to_string();
    let slug = SLUG_RE
        .replace_all(&lowered, "-")
        .trim_matches('-')
        .to_string();
    if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    }
}

fn strip_html_tags(raw: &str) -> String {
    lazy_static! {
        static ref TAG_RE: Regex = Regex::new(r"(?is)<[^>]+>").unwrap();
    }
    TAG_RE.replace_all(raw, "").to_string()
}

fn line_number_from_offset(source: &str, offset: usize) -> usize {
    source[..offset].lines().count() + 1
}

fn detect_frontmatter_issue(source: &str) -> Option<String> {
    let mut lines = source.split_inclusive('\n');
    let first_line = lines.next()?;
    if first_line.trim() != "---" {
        return None;
    }

    let mut block = String::new();
    for line in lines {
        if line.trim() == "---" {
            if serde_yaml::from_str::<serde_yaml::Value>(&block).is_err() {
                return Some("Frontmatter block is not valid YAML".to_string());
            }
            return None;
        }
        block.push_str(line);
    }

    Some("Frontmatter block is not terminated with ---".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn renders_basic_asciidoc() {
        let renderer = DocRenderer::new();
        let src = "= Test Doc\n\nA paragraph with *bold* text.";
        let html = renderer
            .render_asciidoc(src)
            .expect("asciidoc render succeeds");

        assert!(
            html.contains("<p>A paragraph with <strong>bold</strong> text.</p>"),
            "html should include rendered paragraph, got: {html}"
        );
        assert!(
            html.contains("<h1"),
            "doctype title should be present, got: {html}"
        );
    }

    #[test]
    fn reloadable_state_reloads_documents_on_each_call() {
        let temp_root = std::env::temp_dir().join(format!(
            "dossiers-reload-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_millis()
        ));

        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(&temp_root).expect("create temp root");

        let doc_path = temp_root.join("0001-demo.md");
        fs::write(&doc_path, "# First Title\n\nBody").expect("write initial document");

        let state = ReloadableAppState {
            input_path: temp_root.clone(),
            project_root: temp_root.clone(),
            config_path: None,
            assets: Assets::embedded(),
        };

        let first = state.load().expect("initial load should succeed");
        let first_title = first
            .specs_by_id
            .get("0001")
            .expect("spec exists after first load")
            .title
            .clone();

        fs::write(&doc_path, "# Second Title\n\nBody").expect("write updated document");

        let second = state.load().expect("reload should succeed");
        let second_title = second
            .specs_by_id
            .get("0001")
            .expect("spec exists after reload")
            .title
            .clone();

        assert_ne!(first_title, second_title);
        assert_eq!(second_title, "Second Title");

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn uses_stat_times_for_untracked_documents() {
        let temp_root = std::env::temp_dir().join(format!(
            "dossiers-stat-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_millis()
        ));

        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(&temp_root).expect("create temp root");

        let doc_path = temp_root.join("0002-stat.md");
        fs::write(&doc_path, "# Stat Title\n\nBody").expect("write document");

        let state = ReloadableAppState {
            input_path: temp_root.clone(),
            project_root: temp_root.clone(),
            config_path: None,
            assets: Assets::embedded(),
        };

        let loaded = state.load().expect("load should succeed");
        let spec = loaded
            .specs_by_id
            .get("0002")
            .expect("spec should be parsed");

        let created = spec.created.expect("created should come from stat");
        let updated = spec.updated.expect("updated should come from stat");
        assert!(
            updated >= created,
            "updated should be at least created, got created={created}, updated={updated}"
        );

        let _ = fs::remove_dir_all(&temp_root);
    }
}
