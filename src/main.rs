use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use actix_files::Files;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use anyhow::{bail, Context, Result};
use asciidoc_parser::{
    blocks::{
        Block as AsciidocBlock, Break as AsciidocBreak, BreakType, CompoundDelimitedBlock,
        IsBlock as _, MediaBlock, MediaType, RawDelimitedBlock, SectionBlock, SimpleBlock,
        SimpleBlockStyle,
    },
    document::Document as AsciidocDocument,
    Parser as AsciidocParser,
};
use chrono::{Local, NaiveDate, TimeZone};
use lazy_static::lazy_static;
use maud::{html, Markup, PreEscaped};
use pulldown_cmark::{html as md_html, Options as MdOptions, Parser};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use thiserror::Error;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

const EMBEDDED_CSS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/global.css"
));
const EMBEDDED_FAVICON: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/favicon.svg"
));
const THEME_INIT_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/theme-init.js"
));
const THEME_TOGGLE_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/theme-toggle.js"
));
const MINI_TOC_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/mini-toc.js"
));
const INDEX_SEARCH_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/index-search.js"
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
    source: String,
    format: DocFormat,
}

#[derive(Clone)]
struct Assets {
    css_source: CssSource,
    favicon_source: FaviconSource,
    theme_init_source: ScriptSource,
    theme_toggle_source: ScriptSource,
    mini_toc_source: ScriptSource,
    index_search_source: ScriptSource,
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
        }
    }

    fn from_assets_dir(dir: PathBuf) -> Self {
        let css_path = dir.join("global.css");
        let favicon_path = dir.join("favicon.svg");
        let theme_init_path = dir.join("theme-init.js");
        let theme_toggle_path = dir.join("theme-toggle.js");
        let mini_toc_path = dir.join("mini-toc.js");
        let index_search_path = dir.join("index-search.js");

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

        Self {
            css_source,
            favicon_source,
            theme_init_source,
            theme_toggle_source,
            mini_toc_source,
            index_search_source,
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
        Self::read_script(&self.theme_toggle_source, THEME_TOGGLE_SCRIPT, "theme toggle")
    }

    fn mini_toc_script(&self) -> String {
        Self::read_script(&self.mini_toc_source, MINI_TOC_SCRIPT, "mini TOC")
    }

    fn index_search_script(&self) -> String {
        Self::read_script(&self.index_search_source, INDEX_SEARCH_SCRIPT, "index search")
    }
}

#[derive(Clone)]
struct AppState {
    specs: Vec<SpecDocument>,
    specs_by_id: HashMap<String, SpecDocument>,
    spec_ids: HashSet<String>,
    site_name: String,
    assets: Assets,
    renderer: DocRenderer,
}

type StaticMount = (String, PathBuf);

struct LoadResult {
    specs: Vec<SpecDocument>,
    static_mounts: Vec<StaticMount>,
}

#[derive(Debug, Clone)]
struct ParsedMetadata {
    title: String,
    status: String,
    created: Option<i64>,
    updated: Option<i64>,
    authors: Vec<String>,
    links: Vec<Link>,
}

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
    fn into_vec(self) -> Vec<String> {
        match self {
            FrontmatterAuthors::Single(value) => vec![value],
            FrontmatterAuthors::List(values) => values,
        }
    }
}

fn parse_doc_metadata(source: &str, format: &DocFormat) -> ParsedMetadata {
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

    let title = extract_title(source, format);

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

fn parse_doc(source: &str, format: &DocFormat) -> ParsedDoc {
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

            let mut metadata = ParsedMetadata {
                title: frontmatter.title.unwrap_or_default(),
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

            if metadata.title.is_empty() {
                metadata.title = extract_title(&body, format);
            }

            return ParsedDoc { metadata, body };
        }
    }

    let metadata = parse_doc_metadata(source, format);
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

fn load_specs_from_json(path: &Path) -> Result<LoadResult> {
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

fn load_specs_from_directory(dir: &Path) -> Result<LoadResult> {
    if !dir.is_dir() {
        bail!("Provided path is not a directory: {}", dir.display());
    }

    let mut spec_dirs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid directory name under {}", dir.display()))?
            .to_string();
        let Some(id) = extract_spec_id(&dir_name) else {
            continue;
        };
        spec_dirs.push((id, dir_name, path));
    }

    if spec_dirs.is_empty() {
        bail!(
            "No spec directories found in {} (expected subdirectories like 0001-*)",
            dir.display()
        );
    }

    let mut specs = Vec::new();
    let mut static_mounts = Vec::new();

    for (spec_id, dir_name, path) in spec_dirs {
        let (doc_path, format) = find_doc_file(&path)?;
        let source = fs::read_to_string(&doc_path)
            .with_context(|| format!("Reading spec document at {}", doc_path.display()))?;

        let parsed_doc = parse_doc(&source, &format);
        let meta = parsed_doc.metadata;
        let title = if meta.title.is_empty() {
            dir_name.clone()
        } else {
            meta.title.clone()
        };

        let updated_sort = meta
            .updated
            .or(meta.created)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        specs.push(SpecDocument {
            id: spec_id.clone(),
            dir_name,
            title,
            status: meta.status,
            created: meta.created,
            updated: meta.updated.or(meta.created),
            authors: meta.authors,
            links: meta.links,
            updated_sort,
            source: parsed_doc.body,
            format,
        });

        static_mounts.push((format!("/{}", spec_id), path));
    }

    Ok(LoadResult {
        specs,
        static_mounts,
    })
}

fn load_specs(input_path: &Path) -> Result<LoadResult> {
    if input_path.is_dir() {
        load_specs_from_directory(input_path)
    } else {
        load_specs_from_json(input_path)
    }
}

fn load_and_sort_specs(input_path: &Path) -> Result<(Vec<SpecDocument>, Vec<StaticMount>)> {
    let mut load_result = load_specs(input_path)?;
    load_result.specs.sort_by(|a, b| {
        b.updated_sort
            .cmp(&a.updated_sort)
            .then_with(|| b.id.cmp(&a.id))
    });

    Ok((load_result.specs, load_result.static_mounts))
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
    },
}

#[actix_web::main]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let project_dir = project_dir_from_args(&raw_args);
    print_banner(project_dir.as_deref());

    let command = match parse_args(&raw_args) {
        Ok(cmd) => cmd,
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(err) = run_command(command).await {
        eprintln!("{err}");
        std::process::exit(1);
    }

    Ok(())
}

async fn run_command(command: CliCommand) -> Result<()> {
    match command {
        CliCommand::Serve(input_path) => run_server(input_path).await,
        CliCommand::Prepare(input_path) => {
            run_prepare(input_path)?;
            Ok(())
        }
        CliCommand::Build {
            input_path,
            output_dir,
        } => {
            run_build(input_path, output_dir)?;
            Ok(())
        }
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
    let mut args = args.iter();
    let Some(command) = args.next() else {
        return None;
    };

    match command.as_str() {
        "serve" | "prepare" => args.next().map(|path| PathBuf::from(path)),
        "build" => {
            let mut input_path = None;

            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-o" | "--output" => {
                        // Skip the value for --output if present.
                        let _ = args.next();
                    }
                    _ if input_path.is_none() => input_path = Some(PathBuf::from(arg)),
                    _ => {}
                }
            }

            input_path
        }
        _ => None,
    }
}

fn parse_args(args: &[String]) -> Result<CliCommand> {
    let mut args = args.iter();
    let Some(command) = args.next() else {
        bail!("Missing command");
    };

    match command.as_str() {
        "serve" => {
            let path = args
                .next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Missing path for serve"))?;
            if args.next().is_some() {
                bail!("Unexpected argument for serve");
            }
            Ok(CliCommand::Serve(validate_path(path)?))
        }
        "prepare" => {
            let path = args
                .next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Missing path for prepare"))?;
            if args.next().is_some() {
                bail!("Unexpected argument for prepare");
            }
            Ok(CliCommand::Prepare(validate_path(path)?))
        }
        "build" => {
            let mut input_path = None;
            let mut output_dir = None;

            let mut args = args.cloned();

            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-o" | "--output" => {
                        let path = args
                            .next()
                            .ok_or_else(|| anyhow::anyhow!("Missing value for --output"))?;
                        output_dir = Some(PathBuf::from(path));
                    }
                    _ if input_path.is_none() => {
                        input_path = Some(arg);
                    }
                    _ => bail!("Unexpected argument for build: {arg}"),
                }
            }

            let input = input_path
                .ok_or_else(|| anyhow::anyhow!("Missing path for build"))
                .and_then(validate_path)?;
            let output = output_dir.unwrap_or_else(|| PathBuf::from("output"));
            Ok(CliCommand::Build {
                input_path: input,
                output_dir: output,
            })
        }
        _ => bail!("Unknown command: {command}"),
    }
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  dossiers serve <path-to-spec-data.json|path-to-spec-directory>");
    eprintln!("  dossiers prepare <path-to-spec-directory|path-to-spec-data.json>");
    eprintln!("  dossiers build <path-to-spec-directory|path-to-spec-data.json> [-o <output-dir>]");
}

fn validate_path(path: String) -> Result<PathBuf> {
    let input_path = PathBuf::from(path);
    if !input_path.exists() {
        bail!("Spec source not found: {}", input_path.display());
    }
    Ok(input_path)
}

async fn run_server(input_path: PathBuf) -> Result<()> {
    let project_root = project_root();

    let assets = Assets::from_assets_dir(project_root.join("assets"));
    let site_name = resolve_site_name(&project_root);

    let (state, static_mounts) = build_app_state(&input_path, site_name, assets)?;

    println!("Serving specs on http://localhost:8080");
    HttpServer::new(move || {
        let mut app = App::new()
            .app_data(state.clone())
            .route("/", web::get().to(index_page))
            .route("/favicon.svg", web::get().to(favicon))
            .route("/author/{slug}/", web::get().to(author_redirect))
            .route("/author/{slug}", web::get().to(author_page))
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

fn run_prepare(input_path: PathBuf) -> Result<()> {
    let project_root = project_root();
    let (specs, _) = load_and_sort_specs(&input_path)?;

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

fn run_build(input_path: PathBuf, output_dir: PathBuf) -> Result<()> {
    let project_root = project_root();
    let assets = Assets::embedded();
    let site_name = resolve_site_name(&project_root);

    let (state_data, static_mounts) = build_app_state(&input_path, site_name, assets)?;
    let state = state_data.get_ref();

    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)
            .with_context(|| format!("Clearing output directory {}", output_dir.display()))?;
    }
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Creating output directory {}", output_dir.display()))?;

    let mount_map: HashMap<String, PathBuf> = static_mounts.into_iter().collect();

    let index_path = output_dir.join("index.html");
    let index_html = render_index(state, "./").into_string();
    write_html_file(&index_path, index_html)?;
    write_embedded_favicon(&output_dir)?;

    for spec in &state.specs {
        let rendered_html = render_spec_body(state, spec, "".to_string(), "../")?;
        let page = render_spec(state, spec, &rendered_html, "../").into_string();
        let dest = output_dir.join(&spec.id).join("index.html");
        write_html_file(&dest, page)?;

        let asset_paths = collect_doc_assets(&rendered_html);
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
        let page = render_author(state, &name, &authored, "../../").into_string();
        let dest = output_dir.join("author").join(slug).join("index.html");
        write_html_file(&dest, page)?;
    }

    if !index_path.exists() {
        write_html_file(&index_path, render_index(state, "./").into_string())?;
    }

    println!(
        "Static site written to {} (index at {})",
        output_dir.display(),
        index_path.display()
    );
    Ok(())
}

fn build_app_state(
    input_path: &Path,
    site_name: String,
    assets: Assets,
) -> Result<(web::Data<AppState>, Vec<StaticMount>)> {
    let (specs, static_mounts) = load_and_sort_specs(input_path)?;
    let spec_ids = specs.iter().map(|s| s.id.clone()).collect::<HashSet<_>>();
    let renderer = DocRenderer::new();
    let specs_by_id = specs
        .iter()
        .cloned()
        .map(|spec| (spec.id.clone(), spec))
        .collect::<HashMap<_, _>>();

    let state = web::Data::new(AppState {
        specs,
        specs_by_id,
        spec_ids,
        site_name,
        assets,
        renderer,
    });

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

fn collect_doc_assets(html: &str) -> Vec<String> {
    lazy_static! {
        static ref ASSET_RE: Regex =
            Regex::new(r#"(?i)\b(?:src|href)=['"]([^'"]*(?:attachments|images)/[^'">]+)"#).unwrap();
    }

    ASSET_RE
        .captures_iter(html)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .map(|raw| normalize_asset_path(&raw))
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_asset_path(raw: &str) -> String {
    if raw.is_empty() || raw.starts_with('#') || raw.contains("://") {
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
    path
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

async fn favicon(state: web::Data<AppState>) -> impl Responder {
    let favicon = state.assets.favicon();
    HttpResponse::Ok()
        .content_type("image/svg+xml")
        .body(favicon)
}

async fn index_page(state: web::Data<AppState>) -> impl Responder {
    let markup = render_index(&state, "/");
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

async fn spec_redirect(path: web::Path<String>) -> impl Responder {
    let spec_id = path.into_inner();
    HttpResponse::MovedPermanently()
        .append_header(("Location", format!("/{spec_id}")))
        .finish()
}

async fn spec_page(path: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    let spec_id = path.into_inner();
    let Some(spec) = state.specs_by_id.get(&spec_id) else {
        return HttpResponse::Found()
            .append_header(("Location", "/"))
            .finish();
    };

    let rendered_html = match render_spec_body(&state, spec, format!("/{}/", spec.id), "/") {
        Ok(html) => html,
        Err(err) => {
            eprintln!("Failed to render spec {spec_id}: {err:?}");
            return HttpResponse::InternalServerError()
                .body(format!("Failed to render specification {spec_id}: {err:?}"));
        }
    };

    let markup = render_spec(&state, spec, &rendered_html, "/");
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

async fn author_page(path: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    let slug = path.into_inner();
    let authored: Vec<&SpecDocument> = state
        .specs
        .iter()
        .filter(|spec| {
            spec.authors
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

    let markup = render_author(&state, &author_name, &authored, "/");
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_index(state: &AppState, prefix: &str) -> Markup {
    let site_name = &state.site_name;
    let index_search_js = state.assets.index_search_script();
    let content = html! {
        main class="container" {
            section class="hero" {
                h1 { "Specification Library" }
                p { "Browse all specifications documents. Search by title, ID, or author to jump straight to what you need." }
                form class="search-bar" role="search" onsubmit="event.preventDefault();" {
                    label class="sr-only" for="spec-search" { "Search specifications" }
                    div class="search-input" {
                        input id="spec-search" type="search" name="q" placeholder="Search by title, ID, or author" autocomplete="off" autofocus {}
                        span class="search-hint" { "/" }
                    }
                }
            }

            @if state.specs.is_empty() {
                p class="empty-state" { "No specification documents found." }
            } @else {
                ul class="spec-list" {
                    @for spec in &state.specs {
                        li
                            data-title={(spec.title.to_lowercase())}
                            data-id={(spec.id.to_lowercase())}
                            data-authors={(spec.authors.iter().map(|a| a.to_lowercase()).collect::<Vec<_>>().join(" "))}
                        {
                            a class="spec-card" href={(join_prefix(prefix, &spec.id))} {
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
                p class="empty-state filter-empty" hidden { "No specs match this search." }
            }
            }
        script { (PreEscaped(index_search_js)) }
    };

    let css = state.assets.css();
    let theme_init_js = state.assets.theme_init_script();
    let theme_toggle_js = state.assets.theme_toggle_script();
    base_layout(
        site_name,
        site_name,
        "Index of local specification documents",
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
        },
        content,
        prefix,
    )
}

fn render_spec(state: &AppState, spec: &SpecDocument, rendered_html: &str, prefix: &str) -> Markup {
    let title = format!("SPEC-{} {} - {}", spec.id, spec.title, state.site_name);
    let description = format!("Rendered specification {}", spec.dir_name);

    let links: Vec<(&str, &str)> = spec
        .links
        .iter()
        .map(|link| (link.label.as_str(), link.href.as_str()))
        .collect();

    let mini_toc_js = state.assets.mini_toc_script();
    let content = html! {
        main class="container" {
            a class="back-link" href={(join_prefix(prefix, ""))} { "← Back to index" }

            div class="spec-header" {
                span class="meta-label" { "" }
                span class={(format!("tag {}", spec.status.to_lowercase()))} { (&spec.status) }
            }
            div class="spec-header" {
                div class="spec-id-block" { span class="spec-id" { "#" (spec.id) } }
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
    base_layout(
        &state.site_name,
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
        },
        content,
        prefix,
    )
}

fn render_author(
    state: &AppState,
    author_name: &str,
    authored: &[&SpecDocument],
    prefix: &str,
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
                            a class="spec-card" href={(join_prefix(prefix, &spec.id))} {
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
        &title,
        &description,
        LayoutAssets {
            css: &css,
            theme_init_js: &theme_init_js,
            theme_toggle_js: &theme_toggle_js,
        },
        content,
        prefix,
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

struct LayoutAssets<'a> {
    css: &'a str,
    theme_init_js: &'a str,
    theme_toggle_js: &'a str,
}

fn base_layout(
    site_name: &str,
    title: &str,
    description: &str,
    assets: LayoutAssets,
    content: Markup,
    prefix: &str,
) -> Markup {
    let LayoutAssets {
        css,
        theme_init_js,
        theme_toggle_js,
    } = assets;
    let home_href = join_prefix(prefix, "");
    let favicon_href = join_prefix(prefix, "favicon.svg");
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
                        span class="tagline" { "Local index of the specification docs" }
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
                        span { "Powered by Dossiers" }
                    }
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
        source,
        format,
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
    let rewritten_links = rewrite_spec_links(&prefixed_assets, &state.spec_ids, link_prefix);
    Ok(rewritten_links)
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
            let _ = write!(
                buf,
                "<pre><code>{}</code></pre>",
                block.content().rendered()
            );
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
            let _ = write!(
                buf,
                "<pre><code>{}</code></pre>",
                block.content().rendered()
            );
        }
        "listing" => {
            let _ = write!(
                buf,
                "<pre><code>{}</code></pre>",
                block.content().rendered()
            );
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
            Regex::new(r#"(?i)\b(src|href)=([\"'])(\.?/?(attachments|images)/[^\"'>\s]+)"#)
                .unwrap();
    }

    let base = if asset_base.is_empty() || asset_base.ends_with('/') {
        asset_base.to_string()
    } else {
        format!("{asset_base}/")
    };

    ASSET_RE
        .replace_all(html, |caps: &regex::Captures| {
            let attr = &caps[1];
            let quote = &caps[2];
            let value = caps[3].trim_start_matches("./").trim_start_matches('/');
            format!(r#"{attr}={quote}{base}{value}{quote}"#)
        })
        .to_string()
}

fn rewrite_spec_links(html: &str, spec_ids: &HashSet<String>, prefix: &str) -> String {
    lazy_static! {
        static ref HREF_RE: Regex = Regex::new(r#"(?i)\bhref=([\"'])([^\"']+)"#).unwrap();
    }

    HREF_RE
        .replace_all(html, |caps: &regex::Captures| {
            let quote = &caps[1];
            let url = &caps[2];
            let rewritten = normalize_spec_link(url, spec_ids, prefix);
            format!(r#"href={quote}{rewritten}{quote}"#)
        })
        .to_string()
}

fn normalize_spec_link(url: &str, spec_ids: &HashSet<String>, prefix: &str) -> String {
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
            return join_prefix(prefix, format!("{spec_id}{fragment}"));
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

fn extract_title(source: &str, format: &DocFormat) -> String {
    match format {
        DocFormat::Asciidoc => source
            .lines()
            .find(|line| line.trim_start().starts_with('='))
            .map(|line| line.trim_start_matches('=').trim().to_string()),
        DocFormat::Markdown => source
            .lines()
            .find(|line| line.trim_start().starts_with('#'))
            .map(|line| line.trim_start_matches('#').trim().to_string()),
    }
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "Untitled specification".to_string())
}

fn extract_spec_id(name: &str) -> Option<String> {
    lazy_static! {
        static ref ID_RE: Regex = Regex::new(r"^(\d{4,})").unwrap();
    }
    ID_RE
        .captures(name)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
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
            r"(?xi)^(\d{1,4})[/. -](\d{1,2})[/. -](\d{1,4})(?:\s+(\d{1,2}):(\d{2})(?::(\d{2}))?)?$"
        )
        .unwrap();
    }

    let caps = NUMERIC_RE.captures(input.trim())?;
    let first = caps.get(1)?.as_str();
    let second = caps.get(2)?.as_str();
    let third = caps.get(3)?.as_str();
    let hour = caps.get(4).map(|m| m.as_str());
    let minute = caps.get(5).map(|m| m.as_str());
    let second_part = caps.get(6).map(|m| m.as_str());

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

fn resolve_site_name(project_root: &Path) -> String {
    if let Ok(env_name) = std::env::var("SITE_NAME") {
        let trimmed = env_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Some(name) = read_project_config_name(project_root) {
        return name;
    }

    "Dossiers".into()
}

fn read_project_config_name(project_root: &Path) -> Option<String> {
    let config_path = project_root.join("src/generated/project-config.json");
    let file = File::open(config_path).ok()?;
    let value: Value = serde_json::from_reader(file).ok()?;
    value
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}
#[cfg(test)]
mod tests {
    use super::*;

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
}
