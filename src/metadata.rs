use std::collections::HashMap;

use lazy_static::lazy_static;
use pulldown_cmark::{html as md_html, Event, Options as MdOptions, Parser};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};

use crate::{extract_leading_title, normalize_authors, DocFormat, Link};

#[derive(Debug, Clone, Default)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub status: Option<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub authors: Vec<String>,
    pub links: Vec<Link>,
    pub extra: HashMap<String, MetadataValue>,
}

#[derive(Debug, Clone, Default)]
pub struct MetadataReadResult {
    pub metadata: DocumentMetadata,
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectConfiguration {
    #[allow(dead_code)]
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    #[allow(dead_code)]
    pub repository: Option<String>,
    pub subdirectory: Option<String>,
    pub prefix: Option<String>,
    #[allow(dead_code)]
    pub public_access: Option<bool>,
    #[allow(dead_code)]
    pub allowed_github_organizations: Vec<String>,
    #[allow(dead_code)]
    pub allowed_google_workspace_domains: Vec<String>,
    pub statuses: Vec<String>,
    pub default_status: Option<String>,
    #[allow(dead_code)]
    pub new_status: Option<String>,
    pub extra_metadata_fields: Vec<ExtraMetadataField>,
    #[allow(dead_code)]
    pub field_aliases: HashMap<String, String>,
    pub empty_values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExtraMetadataField {
    pub name: String,
    pub type_hint: MetadataValueType,
    #[allow(dead_code)]
    pub required: bool,
    #[allow(dead_code)]
    pub display_name: Option<String>,
    #[allow(dead_code)]
    pub link_format: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum MetadataValueType {
    #[default]
    String,
    Number,
    Boolean,
    Date,
    Markdown,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum MetadataValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Markdown(String),
}

pub struct MetadataReader {
    config: ProjectConfiguration,
    alias_map: HashMap<String, String>,
}

impl MetadataReader {
    pub fn new(config: ProjectConfiguration) -> Self {
        let normalized_empty = config
            .empty_values
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        let mut alias_map = HashMap::new();
        for (target, alias) in &config.field_aliases {
            let target_canonical = canonicalize_key(target);
            if !matches!(
                target_canonical.as_str(),
                "title" | "created" | "updated" | "status"
            ) {
                continue;
            }
            let alias_canonical = canonicalize_key(alias);
            if alias_canonical.is_empty() {
                continue;
            }
            alias_map.insert(alias_canonical, target_canonical.clone());
        }

        Self {
            config: ProjectConfiguration {
                empty_values: if normalized_empty.is_empty() {
                    vec!["n/a".to_string()]
                } else {
                    normalized_empty
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect()
                },
                ..config
            },
            alias_map,
        }
    }

    pub fn read(
        &self,
        source: &str,
        format: DocFormat,
        fallback_title: &str,
    ) -> MetadataReadResult {
        let mut metadata = DocumentMetadata::default();
        let mut body = source.to_string();
        let mut parsed_frontmatter = false;
        let mut parsed_list = false;

        if let Some((frontmatter, remaining)) = self.parse_frontmatter(source, format) {
            self.apply_frontmatter(&mut metadata, &frontmatter);
            body = remaining;
            parsed_frontmatter = true;
        }

        if !parsed_frontmatter {
            if let Some((mut pairs, remaining)) = parse_leading_unordered_list(source) {
                if matches!(format, DocFormat::Markdown) {
                    for (key, value) in pairs.iter_mut() {
                        if self.is_markdown_extra_field(key) {
                            continue;
                        }
                        *value = markdown_plain_text(value);
                    }
                }
                self.apply_pairs(&mut metadata, pairs);
                body = remaining;
                parsed_list = true;
            } else {
                self.apply_attribute_lines(source, &mut metadata);
            }
        }

        if metadata.title.is_none() && parsed_list && !fallback_title.is_empty() {
            metadata.title = Some(fallback_title.to_string());
        }

        if metadata.title.is_none() {
            metadata.title = extract_leading_title(&body, &format)
                .or_else(|| Some(fallback_title.to_string()).filter(|value| !value.is_empty()));
        }

        if metadata.status.is_none() {
            metadata.status = Some(self.default_status());
        }

        metadata.authors = normalize_authors(metadata.authors);

        MetadataReadResult { metadata, body }
    }

    pub(crate) fn default_status(&self) -> String {
        self.config
            .default_status
            .clone()
            .or_else(|| self.config.statuses.first().cloned())
            .unwrap_or_else(|| "DRAFT".to_string())
    }

    fn parse_frontmatter(&self, source: &str, format: DocFormat) -> Option<(YamlMapping, String)> {
        if !matches!(format, DocFormat::Markdown) {
            return None;
        }

        let mut lines = source.split_inclusive('\n');
        let first_line = lines.next()?;
        if first_line.trim() != "---" {
            return None;
        }

        let mut block = String::new();
        let mut consumed = first_line.len();

        for line in lines {
            consumed += line.len();
            if line.trim() == "---" {
                let mapping = parse_frontmatter_block(&block);
                let body = source.get(consumed..).unwrap_or("").to_string();
                return Some((mapping, body));
            }
            block.push_str(line);
        }

        None
    }

    fn apply_frontmatter(&self, metadata: &mut DocumentMetadata, mapping: &YamlMapping) {
        for (key, value) in mapping {
            let Some(key_str) = key.as_str() else {
                continue;
            };
            let canonical = canonicalize_key(key_str);
            if canonical.is_empty() {
                continue;
            }
            let resolved = self.resolve_standard_key(&canonical);
            let mut handled = false;

            match resolved.as_str() {
                "title" => {
                    if let Some(title) =
                        yaml_value_to_string(value).filter(|v| !self.is_empty_value(v))
                    {
                        metadata.title = Some(title);
                        handled = true;
                    }
                }
                "status" => {
                    if let Some(status) =
                        yaml_value_to_string(value).filter(|v| !self.is_empty_value(v))
                    {
                        metadata.status = Some(status);
                        handled = true;
                    }
                }
                "created" => {
                    if let Some(created) =
                        yaml_value_to_string(value).filter(|v| !self.is_empty_value(v))
                    {
                        metadata.created = Some(created);
                        handled = true;
                    }
                }
                "updated" => {
                    if let Some(updated) =
                        yaml_value_to_string(value).filter(|v| !self.is_empty_value(v))
                    {
                        metadata.updated = Some(updated);
                        handled = true;
                    }
                }
                _ => {}
            }

            if handled {
                continue;
            }

            match canonical.as_str() {
                "authors" | "author" => {
                    if let Some(authors) = parse_authors_from_yaml(value) {
                        metadata.authors = authors;
                    }
                    continue;
                }
                "links" => {
                    if let Some(links) = parse_links_from_yaml(value) {
                        metadata.links.extend(links);
                    }
                    continue;
                }
                _ => {}
            }

            if resolved != canonical && is_standard_key(&resolved) {
                continue;
            }
            if is_standard_key(&canonical) {
                continue;
            }

            self.apply_extra_value(metadata, key_str, value);
        }
    }

    fn apply_pairs(&self, metadata: &mut DocumentMetadata, pairs: Vec<(String, String)>) {
        for (key, value) in pairs {
            self.apply_pair(metadata, &key, &value);
        }
    }

    fn apply_pair(&self, metadata: &mut DocumentMetadata, key: &str, value: &str) {
        let canonical = canonicalize_key(key);
        if canonical.is_empty() {
            return;
        }
        let resolved = self.resolve_standard_key(&canonical);

        match resolved.as_str() {
            "title" => {
                if !self.is_empty_value(value) {
                    metadata.title = Some(value.to_string());
                }
                return;
            }
            "status" => {
                if !self.is_empty_value(value) {
                    metadata.status = Some(value.to_string());
                }
                return;
            }
            "created" => {
                if !self.is_empty_value(value) {
                    metadata.created = Some(value.to_string());
                }
                return;
            }
            "updated" | "lastupdated" => {
                if !self.is_empty_value(value) {
                    metadata.updated = Some(value.to_string());
                }
                return;
            }
            "authors" | "author" => {
                metadata.authors.extend(split_authors(value));
                return;
            }
            _ => {}
        }

        if resolved != canonical && is_standard_key(&resolved) {
            return;
        }
        if is_standard_key(&canonical) {
            return;
        }

        self.apply_extra_value_from_str(metadata, &canonical, value);
    }

    fn apply_attribute_lines(&self, source: &str, metadata: &mut DocumentMetadata) {
        for line in source.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with(':') {
                continue;
            }

            let rest = &trimmed[1..];
            let Some((key, raw_value)) = rest.split_once(':') else {
                continue;
            };
            self.apply_pair(metadata, key, raw_value.trim());
        }
    }

    fn apply_extra_value(&self, metadata: &mut DocumentMetadata, key: &str, value: &YamlValue) {
        let Some(field) = self
            .config
            .extra_metadata_fields
            .iter()
            .find(|field| field.matches(key))
            .cloned()
        else {
            return;
        };

        if let Some(parsed) = parse_typed_yaml_value(value, field.type_hint) {
            metadata.extra.insert(field.name, parsed);
        }
    }

    fn apply_extra_value_from_str(
        &self,
        metadata: &mut DocumentMetadata,
        canonical_key: &str,
        value: &str,
    ) {
        let Some(field) = self
            .config
            .extra_metadata_fields
            .iter()
            .find(|field| field.matches_canonical(canonical_key))
            .cloned()
        else {
            return;
        };

        if let Some(parsed) = parse_typed_str_value(value, field.type_hint) {
            metadata.extra.insert(field.name, parsed);
        }
    }

    fn resolve_standard_key(&self, canonical_key: &str) -> String {
        self.alias_map
            .get(canonical_key)
            .cloned()
            .unwrap_or_else(|| canonical_key.to_string())
    }

    fn is_empty_value(&self, value: &str) -> bool {
        let mut normalized = value.trim().to_ascii_lowercase();
        if normalized.starts_with('(') && normalized.ends_with(')') {
            normalized = normalized
                .trim_matches(['(', ')'].as_ref())
                .trim()
                .to_string();
        }
        self.config
            .empty_values
            .iter()
            .any(|v| normalized == v.to_ascii_lowercase())
    }

    fn is_markdown_extra_field(&self, key: &str) -> bool {
        self.config
            .extra_metadata_fields
            .iter()
            .any(|field| field.type_hint == MetadataValueType::Markdown && field.matches(key))
    }
}

impl ExtraMetadataField {
    fn matches(&self, key: &str) -> bool {
        let canonical = canonicalize_key(key);
        self.matches_canonical(&canonical)
    }

    fn matches_canonical(&self, canonical: &str) -> bool {
        if canonicalize_key(&self.name) == canonical {
            return true;
        }
        self.aliases
            .iter()
            .any(|alias| canonicalize_key(alias) == canonical)
    }

    pub fn from_json_value(value: &JsonValue) -> Option<Self> {
        let name = value
            .get("name")
            .and_then(JsonValue::as_str)?
            .trim()
            .to_string();
        if name.is_empty() {
            return None;
        }

        let type_hint = value
            .get("type")
            .and_then(JsonValue::as_str)
            .and_then(|raw| serde_json::from_str::<MetadataValueType>(&format!("\"{raw}\"")).ok())
            .unwrap_or_default();

        let required = value
            .get("required")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let display_name = value
            .get("display_name")
            .or_else(|| value.get("displayName"))
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let link_format = value
            .get("link_format")
            .or_else(|| value.get("linkFormat"))
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let aliases = value
            .get("aliases")
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(JsonValue::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Some(Self {
            name,
            type_hint,
            required,
            display_name,
            link_format,
            aliases,
        })
    }
}

impl ProjectConfiguration {
    pub fn from_json_value(value: &JsonValue) -> Self {
        let name = value
            .get("name")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let title = value
            .get("title")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let description = value
            .get("description")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let repository = value
            .get("repository")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let subdirectory = value
            .get("subdirectory")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let prefix = value
            .get("prefix")
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let public_access = value
            .get("public_access")
            .or_else(|| value.get("publicAccess"))
            .and_then(JsonValue::as_bool);

        let allowed_github_organizations = value
            .get("allowed_github_organizations")
            .or_else(|| value.get("allowedGithubOrganizations"))
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(JsonValue::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let allowed_google_workspace_domains = value
            .get("allowed_google_workspace_domains")
            .or_else(|| value.get("allowedGoogleWorkspaceDomains"))
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(JsonValue::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let statuses = value
            .get("statuses")
            .or_else(|| value.get("statusList"))
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(JsonValue::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let default_status = value
            .get("defaultStatus")
            .or_else(|| value.get("default_status"))
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let new_status = value
            .get("newStatus")
            .or_else(|| value.get("new_status"))
            .and_then(JsonValue::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let extra_metadata_fields = value
            .get("extraMetadataFields")
            .or_else(|| value.get("extra_metadata_fields"))
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(ExtraMetadataField::from_json_value)
                    .collect()
            })
            .unwrap_or_default();

        let field_aliases = value
            .get("field_aliases")
            .or_else(|| value.get("fieldAliases"))
            .and_then(JsonValue::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| {
                        v.as_str()
                            .map(|vv| (k.trim().to_string(), vv.trim().to_string()))
                    })
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let empty_values = value
            .get("empty_values")
            .or_else(|| value.get("emptyValues"))
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(JsonValue::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| vec!["n/a".to_string()]);

        Self {
            name,
            title,
            description,
            repository,
            subdirectory,
            prefix,
            public_access,
            allowed_github_organizations,
            allowed_google_workspace_domains,
            statuses,
            default_status,
            new_status,
            extra_metadata_fields,
            field_aliases,
            empty_values,
        }
    }
}

fn parse_frontmatter_block(block: &str) -> YamlMapping {
    serde_yaml::from_str::<YamlMapping>(block).unwrap_or_else(|_| {
        let cleaned = sanitize_frontmatter_block(block);
        serde_yaml::from_str(&cleaned).unwrap_or_default()
    })
}

fn parse_leading_unordered_list(source: &str) -> Option<(Vec<(String, String)>, String)> {
    let mut pairs = Vec::new();
    let mut consumed = 0usize;
    let mut started = false;
    let mut heading_title: Option<String> = None;
    let mut in_comment_block = false;

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if in_comment_block {
            consumed += line.len();
            if trimmed.contains("-->") {
                in_comment_block = false;
            }
            continue;
        }

        if trimmed.starts_with("<!--") {
            in_comment_block = !trimmed.contains("-->");
            consumed += line.len();
            continue;
        }

        if trimmed.starts_with("//") {
            consumed += line.len();
            continue;
        }

        if trimmed.is_empty() {
            if started {
                consumed += line.len();
                break;
            }
            consumed += line.len();
            continue;
        }

        if heading_title.is_none() {
            if let Some(title) = parse_heading_title(trimmed) {
                heading_title = Some(title);
                consumed += line.len();
                continue;
            }
        }

        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            started = true;
            let content = trimmed[2..].trim();
            if let Some((key, value)) = content.split_once(':') {
                pairs.push((key.trim().to_string(), value.trim().to_string()));
            }
            consumed += line.len();
            continue;
        }

        if started {
            break;
        } else {
            return None;
        }
    }

    if pairs.is_empty() {
        return None;
    }

    if let Some(title) = heading_title {
        pairs.insert(0, ("title".to_string(), title));
    }

    let body = source
        .get(consumed..)
        .unwrap_or("")
        .trim_start_matches('\n')
        .to_string();
    Some((pairs, body))
}

fn parse_heading_title(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes >= 1 {
            let title = trimmed.trim_start_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    } else if trimmed.starts_with('=') {
        let equals = trimmed.chars().take_while(|c| *c == '=').count();
        if equals >= 1 {
            let title = trimmed.trim_start_matches('=').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn canonicalize_key(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn split_authors(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn markdown_plain_text(raw: &str) -> String {
    let mut parts = Vec::new();
    for event in Parser::new(raw) {
        match event {
            Event::Text(text) | Event::Code(text) => {
                parts.push(text.to_string());
            }
            Event::SoftBreak | Event::HardBreak => parts.push(" ".to_string()),
            _ => {}
        }
    }

    parts
        .join("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_authors_from_yaml(value: &YamlValue) -> Option<Vec<String>> {
    match value {
        YamlValue::String(text) => Some(split_authors(text)),
        YamlValue::Sequence(values) => Some(
            values
                .iter()
                .filter_map(YamlValue::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        ),
        _ => None,
    }
}

fn parse_links_from_yaml(value: &YamlValue) -> Option<Vec<Link>> {
    let mapping = value.as_mapping()?;
    let mut links = Vec::new();
    for (label, href) in mapping {
        let Some(label_str) = label.as_str() else {
            continue;
        };
        let Some(href_str) = href.as_str() else {
            continue;
        };

        let label_clean = label_str.trim();
        let href_clean = href_str.trim();
        if label_clean.is_empty() || href_clean.is_empty() {
            continue;
        }

        links.push(Link {
            label: label_clean.to_string(),
            href: href_clean.to_string(),
        });
    }
    Some(links)
}

fn yaml_value_to_string(value: &YamlValue) -> Option<String> {
    match value {
        YamlValue::String(text) => Some(text.trim().to_string()),
        YamlValue::Bool(b) => Some(b.to_string()),
        YamlValue::Number(num) => Some(num.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn parse_typed_yaml_value(value: &YamlValue, kind: MetadataValueType) -> Option<MetadataValue> {
    match kind {
        MetadataValueType::String | MetadataValueType::Date => {
            yaml_value_to_string(value).map(MetadataValue::String)
        }
        MetadataValueType::Boolean => value.as_bool().map(MetadataValue::Boolean),
        MetadataValueType::Number => value
            .as_f64()
            .or_else(|| value.as_i64().map(|v| v as f64))
            .map(MetadataValue::Number),
        MetadataValueType::Markdown => yaml_value_to_string(value)
            .map(|raw| MetadataValue::Markdown(render_markdown_html(&raw))),
    }
}

fn parse_typed_str_value(value: &str, kind: MetadataValueType) -> Option<MetadataValue> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    match kind {
        MetadataValueType::String | MetadataValueType::Date => {
            Some(MetadataValue::String(trimmed.to_string()))
        }
        MetadataValueType::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(MetadataValue::Boolean(true)),
            "false" | "no" | "0" => Some(MetadataValue::Boolean(false)),
            _ => None,
        },
        MetadataValueType::Number => trimmed.parse::<f64>().ok().map(MetadataValue::Number),
        MetadataValueType::Markdown => Some(MetadataValue::Markdown(render_markdown_html(trimmed))),
    }
}

fn is_standard_key(key: &str) -> bool {
    matches!(
        canonicalize_key(key).as_str(),
        "title" | "status" | "created" | "updated" | "lastupdated" | "authors" | "author" | "links"
    )
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

fn render_markdown_html(source: &str) -> String {
    let mut options = MdOptions::empty();
    options.insert(MdOptions::ENABLE_TABLES);
    options.insert(MdOptions::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(source, options);
    let mut html = String::new();
    md_html::push_html(&mut html, parser);
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leading_list() {
        let doc = "- Title: Demo Doc\n- Status: DRAFT\n\nRest of body";
        let reader = MetadataReader::new(ProjectConfiguration::default());
        let result = reader.read(doc, DocFormat::Markdown, "fallback");
        assert_eq!(result.metadata.title.as_deref(), Some("Demo Doc"));
        assert_eq!(result.metadata.status.as_deref(), Some("DRAFT"));
        assert_eq!(result.body.trim_start(), "Rest of body");
    }

    #[test]
    fn honors_field_aliases_for_frontmatter() {
        let doc = r#"---
custom_title: Alias Title
date_written: 2024-02-01
state: REVIEW
---

Body content"#;

        let mut config = ProjectConfiguration::default();
        config
            .field_aliases
            .insert("title".into(), "custom_title".into());
        config
            .field_aliases
            .insert("created".into(), "date_written".into());
        config.field_aliases.insert("status".into(), "state".into());

        let reader = MetadataReader::new(config);
        let result = reader.read(doc, DocFormat::Markdown, "fallback");
        assert_eq!(result.metadata.title.as_deref(), Some("Alias Title"));
        assert_eq!(result.metadata.created.as_deref(), Some("2024-02-01"));
        assert_eq!(result.metadata.status.as_deref(), Some("REVIEW"));
    }

    #[test]
    fn honors_field_aliases_for_attribute_lines() {
        let doc = r#":custom_title: Attr Title
:date_written: 2024-03-10
:state: SHIPPED

Body content
"#;

        let mut config = ProjectConfiguration::default();
        config
            .field_aliases
            .insert("title".into(), "custom_title".into());
        config
            .field_aliases
            .insert("created".into(), "date_written".into());
        config.field_aliases.insert("status".into(), "state".into());

        let reader = MetadataReader::new(config);
        let result = reader.read(doc, DocFormat::Asciidoc, "fallback");
        assert_eq!(result.metadata.title.as_deref(), Some("Attr Title"));
        assert_eq!(result.metadata.created.as_deref(), Some("2024-03-10"));
        assert_eq!(result.metadata.status.as_deref(), Some("SHIPPED"));
    }

    #[test]
    fn parses_heading_then_list_for_metadata() {
        let doc = "# Title From Heading\n- status: RELEASED\n- created: 2024-04-05\n\nContent";
        let reader = MetadataReader::new(ProjectConfiguration::default());
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        assert_eq!(result.metadata.title.as_deref(), Some("Title From Heading"));
        assert_eq!(result.metadata.status.as_deref(), Some("RELEASED"));
        assert_eq!(result.metadata.created.as_deref(), Some("2024-04-05"));
        assert_eq!(result.body.trim_start(), "Content");
    }

    #[test]
    fn parses_non_h1_heading_then_list_for_metadata() {
        let doc = "## Secondary Heading\n- status: RELEASED\n\nBody";
        let reader = MetadataReader::new(ProjectConfiguration::default());
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        assert_eq!(result.metadata.title.as_deref(), Some("Secondary Heading"));
        assert_eq!(result.metadata.status.as_deref(), Some("RELEASED"));
        assert_eq!(result.body.trim_start(), "Body");
    }

    #[test]
    fn markdown_list_values_are_plaintext() {
        let doc = "- title: `Backtick Title`\n- status: **BOLD**\n\nBody";
        let reader = MetadataReader::new(ProjectConfiguration::default());
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        assert_eq!(result.metadata.title.as_deref(), Some("Backtick Title"));
        assert_eq!(result.metadata.status.as_deref(), Some("BOLD"));
    }

    #[test]
    fn markdown_list_preserves_word_boundaries_from_underscores() {
        let doc = "- title: `async_fn_in_trait`\n\nBody";
        let reader = MetadataReader::new(ProjectConfiguration::default());
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        assert_eq!(result.metadata.title.as_deref(), Some("async_fn_in_trait"));
    }

    #[test]
    fn parses_markdown_extra_field_from_yaml_frontmatter() {
        let doc = r#"---
summary: |
  This is **bold** and _emphasized_.
---

Body
"#;

        let mut config = ProjectConfiguration::default();
        config.extra_metadata_fields.push(ExtraMetadataField {
            name: "summary".into(),
            type_hint: MetadataValueType::Markdown,
            required: false,
            display_name: None,
            link_format: None,
            aliases: vec![],
        });

        let reader = MetadataReader::new(config);
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        match result.metadata.extra.get("summary") {
            Some(MetadataValue::Markdown(html)) => {
                assert!(html.contains("<strong>bold</strong>"));
                assert!(html.contains("<em>emphasized</em>"));
            }
            other => panic!("expected markdown extra metadata, got {:?}", other),
        }
    }

    #[test]
    fn parses_markdown_extra_field_from_markdown_frontmatter_list() {
        let doc = "- summary: Intro with **bold** detail\n\nBody";

        let mut config = ProjectConfiguration::default();
        config.extra_metadata_fields.push(ExtraMetadataField {
            name: "summary".into(),
            type_hint: MetadataValueType::Markdown,
            required: false,
            display_name: None,
            link_format: None,
            aliases: vec![],
        });

        let reader = MetadataReader::new(config);
        let result = reader.read(doc, DocFormat::Markdown, "fallback");

        match result.metadata.extra.get("summary") {
            Some(MetadataValue::Markdown(html)) => {
                assert!(html.contains("<strong>bold</strong>"));
                assert!(html.starts_with("<p>Intro with"));
            }
            other => panic!("expected markdown extra metadata, got {:?}", other),
        }
    }
}
