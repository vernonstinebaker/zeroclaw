use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

mod audit;
mod templates;
mod tool_handler;

pub use tool_handler::SkillToolHandler;

const OPEN_SKILLS_REPO_URL: &str = "https://github.com/besoeasy/open-skills";
const OPEN_SKILLS_SYNC_MARKER: &str = ".zeroclaw-open-skills-sync";
const OPEN_SKILLS_SYNC_INTERVAL_SECS: u64 = 60 * 60 * 24 * 7;
const SKILL_DOWNLOAD_POLICY_FILE: &str = ".download-policy.toml";
const SKILLS_SH_HOST: &str = "skills.sh";

const DEFAULT_PRELOADED_SKILL_SOURCES: [(&str, &str); 2] = [
    (
        "find-skills",
        "https://skills.sh/vercel-labs/skills/find-skills",
    ),
    (
        "skill-creator",
        "https://skills.sh/anthropics/skills/skill-creator",
    ),
];

struct BuiltinPreloadedSkill {
    dir_name: &'static str,
    source_url: &'static str,
    markdown: &'static str,
}

const BUILTIN_PRELOADED_SKILLS: [BuiltinPreloadedSkill; 2] = [
    BuiltinPreloadedSkill {
        dir_name: "find-skills",
        source_url: "https://skills.sh/vercel-labs/skills/find-skills",
        markdown: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/find-skills/SKILL.md"
        )),
    },
    BuiltinPreloadedSkill {
        dir_name: "skill-creator",
        source_url: "https://skills.sh/anthropics/skills/skill-creator",
        markdown: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/skill-creator/SKILL.md"
        )),
    },
];

fn default_policy_version() -> u32 {
    1
}

fn default_preloaded_skill_aliases() -> BTreeMap<String, String> {
    DEFAULT_PRELOADED_SKILL_SOURCES
        .iter()
        .map(|(alias, source)| ((*alias).to_string(), (*source).to_string()))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillDownloadPolicy {
    #[serde(default = "default_policy_version")]
    version: u32,
    #[serde(default = "default_preloaded_skill_aliases")]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    trusted_domains: Vec<String>,
    #[serde(default)]
    blocked_domains: Vec<String>,
}

impl Default for SkillDownloadPolicy {
    fn default() -> Self {
        Self {
            version: default_policy_version(),
            aliases: default_preloaded_skill_aliases(),
            trusted_domains: Vec::new(),
            blocked_domains: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillsShSource {
    owner: String,
    repo: String,
    skill: String,
}

impl SkillsShSource {
    fn github_repo_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repo)
    }
}

/// A skill is a user-defined or community-built capability.
/// Skills live in `~/.zeroclaw/workspace/skills/<name>/SKILL.md`
/// and can include tool definitions, prompts, and automation scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(skip)]
    pub location: Option<PathBuf>,
    /// When true, include full skill instructions even in compact prompt mode.
    #[serde(default)]
    pub always: bool,
}

/// A tool defined by a skill (shell command, HTTP call, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    pub name: String,
    pub description: String,
    /// "shell", "http", "script"
    pub kind: String,
    /// The command/URL/script to execute
    pub command: String,
    #[serde(default)]
    pub args: HashMap<String, String>,
}

/// Skill manifest parsed from SKILL.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillManifest {
    skill: SkillMeta,
    #[serde(default)]
    tools: Vec<SkillTool>,
    #[serde(default)]
    prompts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillMeta {
    name: String,
    description: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Load all skills from the workspace skills directory
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    load_skills_with_open_skills_config(workspace_dir, None, None, None, None)
}

/// Load skills using runtime config values (preferred at runtime).
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
        Some(config.skills.allow_scripts),
        Some(&config.skills.trusted_skill_roots),
    )
}

fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
    config_allow_scripts: Option<bool>,
    config_trusted_skill_roots: Option<&[String]>,
) -> Vec<Skill> {
    let mut skills = Vec::new();
    let allow_scripts = config_allow_scripts.unwrap_or(false);
    let trusted_skill_roots =
        resolve_trusted_skill_roots(workspace_dir, config_trusted_skill_roots.unwrap_or(&[]));

    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        skills.extend(load_open_skills(&open_skills_dir));
    }

    skills.extend(load_workspace_skills(
        workspace_dir,
        allow_scripts,
        &trusted_skill_roots,
    ));
    skills
}

fn load_workspace_skills(
    workspace_dir: &Path,
    allow_scripts: bool,
    trusted_skill_roots: &[PathBuf],
) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir, allow_scripts, trusted_skill_roots)
}

fn resolve_trusted_skill_roots(workspace_dir: &Path, raw_roots: &[String]) -> Vec<PathBuf> {
    let home_dir = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let mut resolved = Vec::new();

    for raw in raw_roots {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        let expanded = if trimmed == "~" {
            home_dir.clone().unwrap_or_else(|| PathBuf::from(trimmed))
        } else if let Some(rest) = trimmed
            .strip_prefix("~/")
            .or_else(|| trimmed.strip_prefix("~\\"))
        {
            home_dir
                .as_ref()
                .map(|home| home.join(rest))
                .unwrap_or_else(|| PathBuf::from(trimmed))
        } else {
            PathBuf::from(trimmed)
        };

        let candidate = if expanded.is_relative() {
            workspace_dir.join(expanded)
        } else {
            expanded
        };

        match candidate.canonicalize() {
            Ok(canonical) if canonical.is_dir() => resolved.push(canonical),
            Ok(canonical) => tracing::warn!(
                "ignoring [skills].trusted_skill_roots entry '{}': canonical path is not a directory ({})",
                trimmed,
                canonical.display()
            ),
            Err(err) => tracing::warn!(
                "ignoring [skills].trusted_skill_roots entry '{}': failed to canonicalize {} ({err})",
                trimmed,
                candidate.display()
            ),
        }
    }

    resolved.sort();
    resolved.dedup();
    resolved
}

fn enforce_workspace_skill_symlink_trust(
    path: &Path,
    trusted_skill_roots: &[PathBuf],
) -> Result<()> {
    let canonical_target = path
        .canonicalize()
        .with_context(|| format!("failed to resolve skill symlink target {}", path.display()))?;

    if !canonical_target.is_dir() {
        anyhow::bail!(
            "symlink target is not a directory: {}",
            canonical_target.display()
        );
    }

    if trusted_skill_roots
        .iter()
        .any(|root| canonical_target.starts_with(root))
    {
        return Ok(());
    }

    if trusted_skill_roots.is_empty() {
        anyhow::bail!(
            "symlink target {} is not allowed because [skills].trusted_skill_roots is empty",
            canonical_target.display()
        );
    }

    anyhow::bail!(
        "symlink target {} is outside configured [skills].trusted_skill_roots",
        canonical_target.display()
    );
}

fn load_skills_from_directory(
    skills_dir: &Path,
    allow_scripts: bool,
    trusted_skill_roots: &[PathBuf],
) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                tracing::warn!(
                    "skipping skill entry {}: failed to read metadata ({err})",
                    path.display()
                );
                continue;
            }
        };

        if metadata.file_type().is_symlink() {
            if let Err(err) = enforce_workspace_skill_symlink_trust(&path, trusted_skill_roots) {
                tracing::warn!(
                    "skipping untrusted symlinked skill entry {}: {err}",
                    path.display()
                );
                continue;
            }
        } else if !metadata.is_dir() {
            continue;
        }

        match audit::audit_skill_directory(&path) {
            Ok(report) if report.is_clean() => {}
            Ok(report) => {
                tracing::warn!(
                    "skipping insecure skill directory {}: {}",
                    path.display(),
                    report.summary()
                );
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    "skipping unauditable skill directory {}: {err}",
                    path.display()
                );
                continue;
            }
        }

        // Try SKILL.toml first, then SKILL.md
        let manifest_path = path.join("SKILL.toml");
        let md_path = path.join("SKILL.md");

        if manifest_path.exists() {
            if let Ok(skill) = load_skill_toml(&manifest_path) {
                skills.push(skill);
            }
        } else if md_path.exists() {
            if let Ok(skill) = load_skill_md(&md_path, &path) {
                skills.push(skill);
            }
        }
    }

    skills
}

fn load_open_skills(repo_dir: &Path) -> Vec<Skill> {
    // Modern open-skills layout stores skill packages in `skills/<name>/SKILL.md`.
    // Prefer that structure to avoid treating repository docs (e.g. CONTRIBUTING.md)
    // as executable skills.
    let nested_skills_dir = repo_dir.join("skills");
    if nested_skills_dir.is_dir() {
        return load_skills_from_directory(&nested_skills_dir, allow_scripts, &[]);
    }

    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let is_markdown = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown {
            continue;
        }

        let is_readme = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("README.md"));
        if is_readme {
            continue;
        }

        match audit::audit_open_skill_markdown(&path, repo_dir) {
            Ok(report) if report.is_clean() => {}
            Ok(report) => {
                tracing::warn!(
                    "skipping insecure open-skill file {}: {}",
                    path.display(),
                    report.summary()
                );
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    "skipping unauditable open-skill file {}: {err}",
                    path.display()
                );
                continue;
            }
        }

        if let Ok(skill) = load_open_skill_md(&path) {
            skills.push(skill);
        }
    }

    skills
}

fn parse_open_skills_enabled(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn open_skills_enabled_from_sources(
    config_open_skills_enabled: Option<bool>,
    env_override: Option<&str>,
) -> bool {
    if let Some(raw) = env_override {
        if let Some(enabled) = parse_open_skills_enabled(raw) {
            return enabled;
        }
        if !raw.trim().is_empty() {
            tracing::warn!(
                "Ignoring invalid ZEROCLAW_OPEN_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
            );
        }
    }

    config_open_skills_enabled.unwrap_or(false)
}

fn open_skills_enabled(config_open_skills_enabled: Option<bool>) -> bool {
    let env_override = std::env::var("ZEROCLAW_OPEN_SKILLS_ENABLED").ok();
    open_skills_enabled_from_sources(config_open_skills_enabled, env_override.as_deref())
}

fn resolve_open_skills_dir_from_sources(
    env_dir: Option<&str>,
    config_dir: Option<&str>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    if let Some(env_dir) = env_dir.and_then(parse_dir) {
        return Some(env_dir);
    }
    if let Some(config_dir) = config_dir.and_then(parse_dir) {
        return Some(config_dir);
    }
    home_dir.map(|home| home.join("open-skills"))
}

fn resolve_open_skills_dir(config_open_skills_dir: Option<&str>) -> Option<PathBuf> {
    let env_dir = std::env::var("ZEROCLAW_OPEN_SKILLS_DIR").ok();
    let home_dir = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    resolve_open_skills_dir_from_sources(
        env_dir.as_deref(),
        config_open_skills_dir,
        home_dir.as_deref(),
    )
}

fn ensure_open_skills_repo(
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Option<PathBuf> {
    if !open_skills_enabled(config_open_skills_enabled) {
        return None;
    }

    let repo_dir = resolve_open_skills_dir(config_open_skills_dir)?;

    if !repo_dir.exists() {
        if !clone_open_skills_repo(&repo_dir) {
            return None;
        }
        let _ = mark_open_skills_synced(&repo_dir);
        return Some(repo_dir);
    }

    if should_sync_open_skills(&repo_dir) {
        if pull_open_skills_repo(&repo_dir) {
            let _ = mark_open_skills_synced(&repo_dir);
        } else {
            tracing::warn!(
                "open-skills update failed; using local copy from {}",
                repo_dir.display()
            );
        }
    }

    Some(repo_dir)
}

fn clone_open_skills_repo(repo_dir: &Path) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create open-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let output = Command::new("git")
        .args(["clone", "--depth", "1", OPEN_SKILLS_REPO_URL])
        .arg(repo_dir)
        .output();

    match output {
        Ok(result) if result.status.success() => {
            tracing::info!("initialized open-skills at {}", repo_dir.display());
            true
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to clone open-skills: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git clone for open-skills: {err}");
            false
        }
    }
}

fn pull_open_skills_repo(repo_dir: &Path) -> bool {
    // If user points to a non-git directory via env var, keep using it without pulling.
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["pull", "--ff-only"])
        .output();

    match output {
        Ok(result) if result.status.success() => true,
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to pull open-skills updates: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git pull for open-skills: {err}");
            false
        }
    }
}

fn should_sync_open_skills(repo_dir: &Path) -> bool {
    let marker = repo_dir.join(OPEN_SKILLS_SYNC_MARKER);
    let Ok(metadata) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified_at) = metadata.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified_at) else {
        return true;
    };

    age >= Duration::from_secs(OPEN_SKILLS_SYNC_INTERVAL_SECS)
}

fn mark_open_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPEN_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

/// Load a skill from a SKILL.toml manifest
fn load_skill_toml(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let manifest: SkillManifest = toml::from_str(&content)?;

    Ok(Skill {
        name: manifest.skill.name,
        description: manifest.skill.description,
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        prompts: manifest.prompts,
        location: Some(path.to_path_buf()),
        always: false,
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let (fm, body) = parse_front_matter(&content);
    let mut name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut version = "0.1.0".to_string();
    let mut author: Option<String> = None;

    // If _meta.json exists alongside SKILL.md, use it for name/version/author.
    // This covers skills installed from zip-based registries (e.g. any zip source).
    let meta_path = dir.join("_meta.json");
    if meta_path.exists() {
        if let Ok(raw) = std::fs::read(&meta_path) {
            if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&raw) {
                if let Some(slug) = meta.get("slug").and_then(|v| v.as_str()) {
                    let normalized =
                        normalize_skill_name(slug.split('/').next_back().unwrap_or(slug));
                    if !normalized.is_empty() {
                        name = normalized;
                    }
                }
                if let Some(v) = meta.get("version").and_then(|v| v.as_str()) {
                    version = v.to_string();
                }
                if let Some(owner) = meta.get("ownerId").and_then(|v| v.as_str()) {
                    author = Some(owner.to_string());
                }
            }
        }
    }

    if let Some(fm_name) = fm.get("name") {
        if !fm_name.is_empty() {
            name = fm_name.clone();
        }
    }
    if let Some(fm_version) = fm.get("version") {
        if !fm_version.is_empty() {
            version = fm_version.clone();
        }
    }
    if let Some(fm_author) = fm.get("author") {
        if !fm_author.is_empty() {
            author = Some(fm_author.clone());
        }
    }
    let always = fm_bool(&fm, "always");
    let prompt_body = if body.trim().is_empty() {
        content.clone()
    } else {
        body.to_string()
    };

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        prompts: vec![prompt_body],
        location: Some(path.to_path_buf()),
        always,
    })
}

fn load_open_skill_md(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("open-skill")
        .to_string();

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "open-skills".to_string(),
        author: Some("besoeasy/open-skills".to_string()),
        tags: vec!["open-skills".to_string()],
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        always: false,
    })
}

/// Strip matching single/double quotes from a scalar value.
fn strip_quotes(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Parse optional YAML-like front matter from a SKILL.md body.
/// Returns (front_matter_map, body_without_front_matter).
fn parse_front_matter(content: &str) -> (HashMap<String, String>, &str) {
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return (HashMap::new(), content);
    };
    if first.trim() != "---" {
        return (HashMap::new(), content);
    }

    let mut map = HashMap::new();
    let start = first.len() + 1;
    let mut end = start;
    for line in lines {
        if line.trim() == "---" {
            let body_start = end + line.len() + 1;
            let body = if body_start <= text.len() {
                text[body_start..].trim_start_matches(['\n', '\r'])
            } else {
                ""
            };
            return (map, body);
        }

        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = strip_quotes(value).to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
        end += line.len() + 1;
    }

    // Unclosed block: ignore as plain markdown for safety/backward compatibility.
    (HashMap::new(), content)
}

/// Parse permissive boolean values from front matter.
fn fm_bool(map: &HashMap<String, String>, key: &str) -> bool {
    map.get(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
        .unwrap_or(false)
}

fn extract_description(content: &str) -> String {
    let (fm, body) = parse_front_matter(content);
    if let Some(desc) = fm.get("description") {
        if !desc.trim().is_empty() {
            return desc.trim().to_string();
        }
    }

    body.lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or("No description")
        .trim()
        .to_string()
}

fn append_xml_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

fn write_xml_text_element(out: &mut String, indent: usize, tag: &str, value: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push('<');
    out.push_str(tag);
    out.push('>');
    append_xml_escaped(out, value);
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

fn resolve_skill_location(skill: &Skill, workspace_dir: &Path) -> PathBuf {
    skill.location.clone().unwrap_or_else(|| {
        workspace_dir
            .join("skills")
            .join(&skill.name)
            .join("SKILL.md")
    })
}

fn render_skill_location(skill: &Skill, workspace_dir: &Path, prefer_relative: bool) -> String {
    let location = resolve_skill_location(skill, workspace_dir);
    if prefer_relative {
        if let Ok(relative) = location.strip_prefix(workspace_dir) {
            return relative.display().to_string();
        }
    }
    location.display().to_string()
}

/// Build the "Available Skills" system prompt section with full skill instructions.
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    skills_to_prompt_with_mode(
        skills,
        workspace_dir,
        crate::config::SkillsPromptInjectionMode::Full,
    )
}

/// Build the "Available Skills" system prompt section with configurable verbosity.
pub fn skills_to_prompt_with_mode(
    skills: &[Skill],
    workspace_dir: &Path,
    mode: crate::config::SkillsPromptInjectionMode,
) -> String {
    use std::fmt::Write;

    if skills.is_empty() {
        return String::new();
    }

    let mut prompt = match mode {
        crate::config::SkillsPromptInjectionMode::Full => String::from(
            "## Available Skills\n\n\
             Skill instructions and tool metadata are preloaded below.\n\
             Follow these instructions directly; do not read skill files at runtime unless the user asks.\n\n\
             <available_skills>\n",
        ),
        crate::config::SkillsPromptInjectionMode::Compact => String::from(
            "## Available Skills\n\n\
             Skill summaries are preloaded below to keep context compact.\n\
             Skill instructions are loaded on demand: read the skill file in `location` when needed. \
             Skills marked `always` include full instructions below even in compact mode.\n\n\
             <available_skills>\n",
        ),
    };

    for skill in skills {
        let _ = writeln!(prompt, "  <skill>");
        write_xml_text_element(&mut prompt, 4, "name", &skill.name);
        write_xml_text_element(&mut prompt, 4, "description", &skill.description);
        let location = render_skill_location(
            skill,
            workspace_dir,
            matches!(mode, crate::config::SkillsPromptInjectionMode::Compact),
        );
        write_xml_text_element(&mut prompt, 4, "location", &location);

        let inject_full =
            matches!(mode, crate::config::SkillsPromptInjectionMode::Full) || skill.always;
        if inject_full {
            if !skill.prompts.is_empty() {
                let _ = writeln!(prompt, "    <instructions>");
                for instruction in &skill.prompts {
                    write_xml_text_element(&mut prompt, 6, "instruction", instruction);
                }
                let _ = writeln!(prompt, "    </instructions>");
            }

            if !skill.tools.is_empty() {
                let _ = writeln!(prompt, "    <tools>");
                for tool in &skill.tools {
                    let _ = writeln!(prompt, "      <tool>");
                    write_xml_text_element(&mut prompt, 8, "name", &tool.name);
                    write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                    write_xml_text_element(&mut prompt, 8, "kind", &tool.kind);
                    let _ = writeln!(prompt, "      </tool>");
                }
                let _ = writeln!(prompt, "    </tools>");
            }
        }

        let _ = writeln!(prompt, "  </skill>");
    }

    prompt.push_str("</available_skills>");
    prompt
}

/// Get the skills directory path
pub fn skills_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("skills")
}

/// Create tool handlers for all skill tools
pub fn create_skill_tools(
    skills: &[Skill],
    security: std::sync::Arc<crate::security::SecurityPolicy>,
) -> Vec<Box<dyn crate::tools::Tool>> {
    let mut tools: Vec<Box<dyn crate::tools::Tool>> = Vec::new();

    for skill in skills {
        for tool_def in &skill.tools {
            match SkillToolHandler::new(skill.name.clone(), tool_def.clone(), security.clone()) {
                Ok(handler) => {
                    tracing::debug!(
                        skill = %skill.name,
                        tool = %tool_def.name,
                        "Registered skill tool"
                    );
                    tools.push(Box::new(handler));
                }
                Err(e) => {
                    tracing::warn!(
                        skill = %skill.name,
                        tool = %tool_def.name,
                        error = %e,
                        "Failed to create skill tool handler"
                    );
                }
            }
        }
    }

    tools
}
}

/// Initialize the skills directory with a README
pub fn init_skills_dir(workspace_dir: &Path) -> Result<()> {
    let dir = skills_dir(workspace_dir);
    std::fs::create_dir_all(&dir)?;

    let readme = dir.join("README.md");
    if !readme.exists() {
        std::fs::write(
            &readme,
            "# ZeroClaw Skills\n\n\
             Each subdirectory is a skill. Create a `SKILL.toml` or `SKILL.md` file inside.\n\n\
             ## SKILL.toml format\n\n\
             ```toml\n\
             [skill]\n\
             name = \"my-skill\"\n\
             description = \"What this skill does\"\n\
             version = \"0.1.0\"\n\
             author = \"your-name\"\n\
             tags = [\"productivity\", \"automation\"]\n\n\
             [[tools]]\n\
             name = \"my_tool\"\n\
             description = \"What this tool does\"\n\
             kind = \"shell\"\n\
             command = \"echo hello\"\n\
             ```\n\n\
             ## SKILL.md format (simpler)\n\n\
             Just write a markdown file with instructions for the agent.\n\
             The agent will read it and follow the instructions.\n\n\
             ## Installing community skills\n\n\
             ```bash\n\
             zeroclaw skills install <source>\n\
             zeroclaw skills list\n\
             ```\n",
        )?;
    }

    ensure_builtin_preloaded_skills(&dir)?;
    let _ = load_or_init_skill_download_policy(&dir)?;

    Ok(())
}

fn is_git_source(source: &str) -> bool {
    is_git_scheme_source(source, "https://")
        || is_git_scheme_source(source, "http://")
        || is_git_scheme_source(source, "ssh://")
        || is_git_scheme_source(source, "git://")
        || is_git_scp_source(source)
}

fn is_git_scheme_source(source: &str, scheme: &str) -> bool {
    let Some(rest) = source.strip_prefix(scheme) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }

    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
}

fn is_git_scp_source(source: &str) -> bool {
    // SCP-like syntax accepted by git, e.g. git@host:owner/repo.git
    // Keep this strict enough to avoid treating local paths as git remotes.
    let Some((user_host, remote_path)) = source.split_once(':') else {
        return false;
    };
    if remote_path.is_empty() {
        return false;
    }
    if source.contains("://") {
        return false;
    }

    let Some((user, host)) = user_host.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !user.contains('/')
        && !user.contains('\\')
        && !host.contains('/')
        && !host.contains('\\')
}

fn normalize_skills_sh_dir_name(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn parse_skills_sh_source(source: &str) -> Option<SkillsShSource> {
    let rest = source.strip_prefix("https://")?;
    let rest = rest.strip_prefix(SKILLS_SH_HOST)?;
    let path = rest
        .trim_start_matches('/')
        .split(&['?', '#'][..])
        .next()
        .unwrap_or("");
    let mut segments = path.split('/').filter(|part| !part.trim().is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    let skill = segments.next()?;
    if owner.contains("..")
        || repo.contains("..")
        || skill.contains("..")
        || owner.contains('\\')
        || repo.contains('\\')
        || skill.contains('\\')
    {
        return None;
    }
    Some(SkillsShSource {
        owner: owner.to_string(),
        repo: repo.to_string(),
        skill: skill.to_string(),
    })
}

fn is_skills_sh_source(source: &str) -> bool {
    parse_skills_sh_source(source).is_some()
}

fn snapshot_skill_children(skills_path: &Path) -> Result<HashSet<PathBuf>> {
    let mut paths = HashSet::new();
    for entry in std::fs::read_dir(skills_path)? {
        let entry = entry?;
        paths.insert(entry.path());
    }
    Ok(paths)
}

fn detect_newly_installed_directory(
    skills_path: &Path,
    before: &HashSet<PathBuf>,
) -> Result<PathBuf> {
    let mut created = Vec::new();
    for entry in std::fs::read_dir(skills_path)? {
        let entry = entry?;
        let path = entry.path();
        if !before.contains(&path) && path.is_dir() {
            created.push(path);
        }
    }

    match created.len() {
        1 => Ok(created.remove(0)),
        0 => anyhow::bail!(
            "Unable to determine installed skill directory after clone (no new directory found)"
        ),
        _ => anyhow::bail!(
            "Unable to determine installed skill directory after clone (multiple new directories found)"
        ),
    }
}

fn enforce_skill_security_audit(skill_path: &Path) -> Result<audit::SkillAuditReport> {
    let report = audit::audit_skill_directory(skill_path)?;
    if report.is_clean() {
        return Ok(report);
    }

    anyhow::bail!("Skill security audit failed: {}", report.summary());
}

fn remove_git_metadata(skill_path: &Path) -> Result<()> {
    let git_dir = skill_path.join(".git");
    if git_dir.exists() {
        std::fs::remove_dir_all(&git_dir)
            .with_context(|| format!("failed to remove {}", git_dir.display()))?;
    }
    Ok(())
}

fn copy_dir_recursive_secure(src: &Path, dest: &Path) -> Result<()> {
    let src_meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("failed to read metadata for {}", src.display()))?;
    if src_meta.file_type().is_symlink() {
        anyhow::bail!(
            "Refusing to copy symlinked skill source path: {}",
            src.display()
        );
    }
    if !src_meta.is_dir() {
        anyhow::bail!("Skill source must be a directory: {}", src.display());
    }

    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create destination {}", dest.display()))?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&src_path)
            .with_context(|| format!("failed to read metadata for {}", src_path.display()))?;

        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Refusing to copy symlink within skill source: {}",
                src_path.display()
            );
        }

        if metadata.is_dir() {
            copy_dir_recursive_secure(&src_path, &dest_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy skill file from {} to {}",
                    src_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn install_local_skill_source(source: &str, skills_path: &Path) -> Result<(PathBuf, usize)> {
    let source_path = PathBuf::from(source);
    if !source_path.exists() {
        anyhow::bail!("Source path does not exist: {source}");
    }

    let source_path = source_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source path {source}"))?;
    let _ = enforce_skill_security_audit(&source_path)?;

    let name = source_path
        .file_name()
        .context("Source path must include a directory name")?;
    let dest = skills_path.join(name);
    if dest.exists() {
        anyhow::bail!("Destination skill already exists: {}", dest.display());
    }

    if let Err(err) = copy_dir_recursive_secure(&source_path, &dest) {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    match enforce_skill_security_audit(&dest) {
        Ok(report) => Ok((dest, report.files_scanned)),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(err)
        }
    }
}

fn install_git_skill_source(source: &str, skills_path: &Path) -> Result<(PathBuf, usize)> {
    let before = snapshot_skill_children(skills_path)?;
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", source])
        .current_dir(skills_path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git clone failed: {stderr}");
    }

    let installed_dir = detect_newly_installed_directory(skills_path, &before)?;
    remove_git_metadata(&installed_dir)?;
    match enforce_skill_security_audit(&installed_dir) {
        Ok(report) => Ok((installed_dir, report.files_scanned)),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&installed_dir);
            Err(err)
        }
    }
}

fn install_skills_sh_source(source: &str, skills_path: &Path) -> Result<(PathBuf, usize)> {
    let parsed = parse_skills_sh_source(source).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid skills.sh source '{source}': expected https://skills.sh/<owner>/<repo>/<skill>"
        )
    })?;

    let repo_url = parsed.github_repo_url();
    let checkout_root = tempfile::tempdir().context("failed to create temporary checkout dir")?;
    let checkout_dir = checkout_root.path().join("repo");

    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", &repo_url])
        .arg(&checkout_dir)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to clone skills.sh repository {repo_url}: {stderr}");
    }

    let candidate_paths = [
        checkout_dir.join("skills").join(&parsed.skill),
        checkout_dir.join(&parsed.skill),
    ];
    let source_dir = candidate_paths
        .iter()
        .find(|candidate| {
            candidate.join("SKILL.md").exists() || candidate.join("SKILL.toml").exists()
        })
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not locate skill '{}' in repository {} (checked skills/{}/ and {}/)",
                parsed.skill,
                repo_url,
                parsed.skill,
                parsed.skill
            )
        })?;

    let normalized_name = normalize_skills_sh_dir_name(&parsed.skill);
    if normalized_name.is_empty() {
        anyhow::bail!(
            "invalid skill name '{}' derived from skills.sh URL: {source}",
            parsed.skill
        );
    }
    let dest = skills_path.join(&normalized_name);
    if dest.exists() {
        anyhow::bail!("Destination skill already exists: {}", dest.display());
    }

    if let Err(err) = copy_dir_recursive_secure(&source_dir, &dest) {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    let meta = serde_json::json!({
        "slug": format!("{}/{}", parsed.owner, parsed.skill),
        "version": "skills.sh",
        "ownerId": parsed.owner,
        "source": source,
    });
    if let Err(err) = std::fs::write(
        dest.join("_meta.json"),
        serde_json::to_vec_pretty(&meta).context("failed to serialize skills.sh metadata")?,
    ) {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err).context("failed to persist skills.sh metadata");
    }

    match enforce_skill_security_audit(&dest) {
        Ok(report) => Ok((dest, report.files_scanned)),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(err)
        }
    }
}

/// Minimal JSON shape returned by the ZeroMarket registry package index endpoint.
#[derive(Debug, serde::Deserialize)]
struct RegistryPackageIndex {
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tools: Vec<RegistryToolEntry>,
    /// Optional CDN base URL where WASM artifacts are hosted.
    ///
    /// When present, artifact URLs may use this host instead of (or in addition
    /// to) the registry host.  The declared host must itself be HTTPS; the client
    /// validates that each artifact URL's host matches either the registry host or
    /// this declared base host.  This lets the registry operator store binaries on
    /// a separate CDN (e.g. Cloudflare R2) without client changes.
    #[serde(default)]
    artifact_base_url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RegistryToolEntry {
    name: String,
    wasm_url: String,
    manifest_url: String,
}

/// Blocking HTTP GET using the system `curl` binary (avoids adding a sync HTTP
/// Extract the hostname from an `https://` URL (the part before the first
/// `'/'`, `'?'`, `'#'`, or `':'` after the scheme).
fn extract_url_host(url: &str) -> &str {
    url.strip_prefix("https://")
        .unwrap_or("")
        .split(&['/', '?', '#', ':'][..])
        .next()
        .unwrap_or("")
}

/// Validate that an artifact URL (wasm_url / manifest_url from the registry index)
/// is HTTPS and served from an allowed host, preventing SSRF via a malicious
/// registry response redirecting downloads to internal hosts.
///
/// Allowed hosts:
/// 1. The registry host itself (e.g. `zeromarket.vercel.app`).
/// 2. The declared `artifact_base_url` host from the package index — lets the
///    registry operator store binaries on a separate CDN (e.g. Cloudflare R2)
///    without hardcoding CDN domains in the client.  The declared base URL must
///    also use HTTPS; otherwise it is ignored.
fn validate_artifact_url(
    artifact_url: &str,
    registry_url: &str,
    artifact_base_url: Option<&str>,
) -> Result<()> {
    if !artifact_url.starts_with("https://") {
        anyhow::bail!("artifact URL must use HTTPS: {artifact_url}");
    }
    let registry_host = extract_url_host(registry_url);
    let artifact_host = extract_url_host(artifact_url);

    if registry_host.is_empty() || artifact_host.is_empty() {
        anyhow::bail!(
            "could not determine host from artifact URL '{}' or registry URL '{}'",
            artifact_url,
            registry_url
        );
    }

    if artifact_host == registry_host {
        return Ok(());
    }

    // Allow host declared by the registry as its artifact CDN, if that
    // declaration is itself a valid HTTPS URL.
    if let Some(base) = artifact_base_url {
        if base.starts_with("https://") {
            let base_host = extract_url_host(base);
            if !base_host.is_empty() && artifact_host == base_host {
                return Ok(());
            }
        }
    }

    // Allow Cloudflare R2 public bucket hostnames (`*.r2.dev`) as a trusted CDN
    // fallback.  R2 is a hosted object-storage service with no access to private
    // networks, so allowing artifacts from any R2 bucket does not create SSRF risk.
    // Registries that store binaries on R2 without declaring `artifact_base_url`
    // still work out of the box.
    if artifact_host.ends_with(".r2.dev") {
        return Ok(());
    }

    anyhow::bail!(
        "artifact host '{}' is not allowed (registry host: '{}'; declared artifact host: '{}')",
        artifact_host,
        registry_host,
        artifact_base_url
            .and_then(|u| {
                if u.starts_with("https://") {
                    Some(extract_url_host(u))
                } else {
                    None
                }
            })
            .unwrap_or("none")
    );
}

// ─── ClawhHub skill installer ────────────────────────────────────────────────
//
// ClawhHub (https://clawhub.ai) is the OpenClaw skill registry.
// Supported source formats:
//   - `https://clawhub.ai/<owner>/<slug>`  (profile URL, auto-detected by domain)
//   - `clawhub:<slug>`                     (short prefix)
//
// The download URL is: https://clawhub.ai/api/v1/download?slug=<slug>
// Zip contents follow the OpenClaw convention: `_meta.json` + `SKILL.md` + scripts.

const CLAWHUB_DOMAIN: &str = "clawhub.ai";
const CLAWHUB_DOWNLOAD_API: &str = "https://clawhub.ai/api/v1/download";

/// Returns true if `source` is a ClawhHub skill reference.
fn is_clawhub_source(source: &str) -> bool {
    if source.starts_with("clawhub:") {
        return true;
    }
    // Auto-detect from domain: https://clawhub.ai/...
    if let Some(rest) = source.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        return host == CLAWHUB_DOMAIN;
    }
    false
}

/// Convert a ClawhHub source string into the zip download URL.
///
/// - `clawhub:gog`                       → `https://clawhub.ai/api/v1/download?slug=gog`
/// - `https://clawhub.ai/steipete/gog`   → `https://clawhub.ai/api/v1/download?slug=steipete/gog`
/// - `https://clawhub.ai/gog`            → `https://clawhub.ai/api/v1/download?slug=gog`
///
/// For profile URLs the full path (owner/slug) is forwarded verbatim as the slug query
/// parameter so the ClawhHub API can resolve owner-namespaced skills correctly.
fn clawhub_download_url(source: &str) -> Result<String> {
    // Short prefix: clawhub:<slug>
    if let Some(slug) = source.strip_prefix("clawhub:") {
        let slug = slug.trim().trim_end_matches('/');
        if slug.is_empty() || slug.contains('/') {
            anyhow::bail!(
                "invalid clawhub source '{}': expected 'clawhub:<slug>' (no slashes in slug)",
                source
            );
        }
        return Ok(format!("{CLAWHUB_DOWNLOAD_API}?slug={slug}"));
    }
    // Profile URL: https://clawhub.ai/<owner>/<slug>  or  https://clawhub.ai/<slug>
    // Forward the full path as the slug so the API can resolve owner-namespaced skills.
    if let Some(rest) = source.strip_prefix("https://") {
        let path = rest
            .strip_prefix(CLAWHUB_DOMAIN)
            .unwrap_or("")
            .trim_start_matches('/');
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            anyhow::bail!("could not extract slug from ClawhHub URL: {source}");
        }
        // Keep the literal slash so the API receives `slug=owner/name`
        // (some backends do not decode %2F in query parameters).
        return Ok(format!("{CLAWHUB_DOWNLOAD_API}?slug={path}"));
    }
    anyhow::bail!("unrecognised ClawhHub source format: {source}")
}

// ─── Generic zip-URL skill installer ─────────────────────────────────────────
//
// Installs a skill from any HTTPS URL that returns a zip archive.
// Supports two source formats:
//   - `zip:https://example.com/path/to/skill.zip`  (explicit prefix)
//   - `https://example.com/skill.zip`              (`.zip` suffix auto-detection)
//
// No system-level `unzip` binary is required; extraction is done in-process
// using the `zip` crate. This makes the feature portable and dependency-free.
//
// If the zip contains a `_meta.json` at its root (OpenClaw registry convention),
// the name, version, and author fields are read from it. Otherwise the skill
// name is derived from the URL's last path segment.

/// Returns true if `source` should be handled as a zip-URL download.
fn is_zip_url_source(source: &str) -> bool {
    // Explicit `zip:https://...` prefix
    if let Some(rest) = source.strip_prefix("zip:") {
        return rest.starts_with("https://");
    }
    // Direct HTTPS URL ending in `.zip`
    let path_part = source.split('?').next().unwrap_or(source);
    source.starts_with("https://") && path_part.ends_with(".zip")
}

/// Strips the `zip:` prefix if present, returning the bare HTTPS URL.
fn zip_url_from_source(source: &str) -> &str {
    source.strip_prefix("zip:").unwrap_or(source)
}

/// Normalize a raw slug or filename into a valid skill directory name.
/// Lowercases, replaces hyphens with underscores, strips everything else.
fn normalize_skill_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Read skill metadata (name, version, author) from a zip archive.
///
/// Checks for `_meta.json` at the root of the archive first (OpenClaw/ClawhHub
/// convention). Falls back to the URL-derived name passed via `url_hint`.
fn extract_zip_skill_meta(
    bytes: &[u8],
    url_hint: &str,
) -> Result<(String, String, Option<String>)> {
    use std::io::Read as _;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).context("downloaded content is not a valid zip archive")?;

    if let Ok(mut f) = archive.by_name("_meta.json") {
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok();
        if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&buf) {
            let slug_raw = meta.get("slug").and_then(|v| v.as_str()).unwrap_or("");
            let base = slug_raw.split('/').next_back().unwrap_or(slug_raw);
            let name = normalize_skill_name(base);
            if !name.is_empty() {
                let version = meta
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0.1.0")
                    .to_string();
                let author = meta
                    .get("ownerId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                return Ok((name, version, author));
            }
        }
    }

    // Fallback: derive name from the URL path (strip query string and .zip suffix)
    let url_path = url_hint.split('?').next().unwrap_or(url_hint);
    let last_seg = url_path.rsplit('/').next().unwrap_or("skill");
    let base = last_seg.strip_suffix(".zip").unwrap_or(last_seg);
    let name = normalize_skill_name(base);
    let name = if name.is_empty() {
        "skill".to_string()
    } else {
        name
    };
    Ok((name, "0.1.0".to_string(), None))
}

/// Install a skill from a local `.zip` file (e.g. downloaded manually from ClawhHub).
///
/// Usage: `zeroclaw skill install /path/to/skill.zip`
fn install_local_zip_source(zip_path: &Path, skills_path: &Path) -> Result<(PathBuf, usize)> {
    let bytes = std::fs::read(zip_path)
        .with_context(|| format!("failed to read zip file: {}", zip_path.display()))?;
    let hint = zip_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("skill.zip");
    extract_zip_bytes_to_skills(&bytes, hint, skills_path)
}

/// Download a zip archive from `url` and install it as a skill under `skills_path`.
///
/// `auth_token` is an optional Bearer token added as `Authorization: Bearer <token>`.
/// Extraction is done in-process (no `unzip` binary required).
/// Returns the installed skill directory path and the number of files written.
fn install_zip_url_source(
    url: &str,
    skills_path: &Path,
    auth_token: Option<&str>,
) -> Result<(PathBuf, usize)> {
    let bytes = fetch_url_blocking(url, auth_token)
        .with_context(|| format!("failed to fetch zip from {url}"))?;
    extract_zip_bytes_to_skills(&bytes, url, skills_path)
}

/// Core zip extraction logic shared by local and remote zip installers.
///
/// Runs a full security audit on the zip contents before extracting a single byte.
/// `name_hint` is used as a fallback for skill name detection (URL or filename).
fn extract_zip_bytes_to_skills(
    bytes: &[u8],
    name_hint: &str,
    skills_path: &Path,
) -> Result<(PathBuf, usize)> {
    // ── Security audit BEFORE extraction ────────────────────────────────────
    // Runs zip-specific checks: entry count, path traversal, native binaries,
    // per-file and total decompressed size limits, compression ratio (zip bomb),
    // and high-risk shell pattern detection in text files.
    let audit_report =
        audit::audit_zip_bytes(bytes).context("zip pre-extraction security check failed")?;
    if !audit_report.is_clean() {
        let findings = audit_report
            .findings
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "zip skill rejected by security audit ({} finding{}):\n{findings}",
            audit_report.findings.len(),
            if audit_report.findings.len() == 1 {
                ""
            } else {
                "s"
            }
        );
    }

    let (skill_name, skill_version, skill_author) = extract_zip_skill_meta(bytes, name_hint)
        .with_context(|| format!("could not determine skill name from zip: {name_hint}"))?;

    let skill_dir = skills_path.join(&skill_name);
    if skill_dir.exists() {
        anyhow::bail!(
            "skill '{}' already exists at {}; run 'zeroclaw skill remove {}' first",
            skill_name,
            skill_dir.display(),
            skill_name
        );
    }
    std::fs::create_dir_all(&skill_dir)?;

    // Extract zip entries
    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).context("failed to re-open zip archive for extraction")?;

    let mut files_written = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = entry.name().to_string();

        // Security: reject path traversal attempts
        if raw_name.contains("..") || raw_name.starts_with('/') {
            let _ = std::fs::remove_dir_all(&skill_dir);
            anyhow::bail!("zip entry contains unsafe path: {raw_name}");
        }

        let out_path = skill_dir.join(&raw_name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = std::fs::File::create(&out_path)
                .with_context(|| format!("failed to create {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut out_file)?;
            files_written += 1;
        }
    }

    // Write a minimal SKILL.toml so the skill appears in `zeroclaw skill list`
    // (only if neither SKILL.toml nor SKILL.md was included in the zip)
    let toml_path = skill_dir.join("SKILL.toml");
    if !toml_path.exists() && !skill_dir.join("SKILL.md").exists() {
        let author_line = skill_author
            .map(|a| format!("author = \"{a}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            &toml_path,
            format!(
                "[skill]\nname = \"{skill_name}\"\ndescription = \"Zip-installed skill\"\nversion = \"{skill_version}\"\n{author_line}"
            ),
        )?;
        files_written += 1;
    }

    Ok((skill_dir, files_written))
}

/// crate to this sync code path). Falls back to a basic TCP approach is not needed
/// because `curl` is universally available on target platforms.
///
/// `auth_token` — if `Some`, adds `Authorization: Bearer <token>` to the request.
fn fetch_url_blocking(url: &str, auth_token: Option<&str>) -> Result<Vec<u8>> {
    // Validate URL scheme — only https:// allowed to prevent SSRF
    if !url.starts_with("https://") {
        anyhow::bail!("registry URL must use HTTPS: {url}");
    }

    // Use --write-out to append the HTTP status code on a separate line so we
    // can give actionable error messages (e.g. 429 rate-limit guidance) without
    // needing a separate HEAD request.
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--max-redirs",
        "5",
        "--max-time",
        "30",
        "--write-out",
        "\n%{http_code}",
    ]);
    if let Some(token) = auth_token {
        cmd.args(["-H", &format!("Authorization: Bearer {token}")]);
    }
    cmd.arg(url);

    let output = cmd
        .output()
        .context("failed to run 'curl' — ensure curl is installed")?;

    // Parse the HTTP status code appended by --write-out.
    let stdout = output.stdout;
    let (body, http_status) = if let Some(nl) = stdout.iter().rposition(|&b| b == b'\n') {
        let code_bytes = stdout[nl + 1..]
            .iter()
            .copied()
            .take_while(|b| b.is_ascii_digit())
            .collect::<Vec<_>>();
        let status: u16 = String::from_utf8_lossy(&code_bytes).parse().unwrap_or(0);
        (stdout[..nl].to_vec(), status)
    } else {
        (stdout, 0)
    };

    if http_status == 429 {
        anyhow::bail!(
            "ClawhHub rate limit reached (HTTP 429). \
             Wait a moment and retry, or set `clawhub_token` in the `[skills]` section \
             of your config.toml to use authenticated requests."
        );
    }

    if !output.status.success() || (http_status != 0 && http_status >= 400) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if http_status != 0 {
            anyhow::bail!("HTTP {http_status} from {url}: {stderr}");
        }
        anyhow::bail!("curl failed for {url}: {stderr}");
    }

    Ok(body)
}

// ─── Handle command ───────────────────────────────────────────────────────────

/// Handle the `skills` CLI command
#[allow(clippy::too_many_lines)]
pub fn handle_command(command: crate::SkillCommands, config: &crate::config::Config) -> Result<()> {
    let workspace_dir = &config.workspace_dir;
    match command {
        crate::SkillCommands::List => {
            let skills = load_skills_with_config(workspace_dir, config);
            if skills.is_empty() {
                println!("No skills installed.");
                println!();
                println!("  Create one: mkdir -p ~/.zeroclaw/workspace/skills/my-skill");
                println!("              echo '# My Skill' > ~/.zeroclaw/workspace/skills/my-skill/SKILL.md");
                println!();
                println!("  Or install: zeroclaw skills install <source>");
            } else {
                println!("Installed skills ({}):", skills.len());
                println!();
                for skill in &skills {
                    println!(
                        "  {} {} — {}",
                        console::style(&skill.name).white().bold(),
                        console::style(format!("v{}", skill.version)).dim(),
                        skill.description
                    );
                    if !skill.tools.is_empty() {
                        println!(
                            "    Tools: {}",
                            skill
                                .tools
                                .iter()
                                .map(|t| t.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !skill.tags.is_empty() {
                        println!("    Tags:  {}", skill.tags.join(", "));
                    }
                }
            }
            println!();
            Ok(())
        }
        crate::SkillCommands::Audit { source } => {
            let source_path = PathBuf::from(&source);
            let target = if source_path.exists() {
                source_path
            } else {
                skills_dir(workspace_dir).join(&source)
            };

            if !target.exists() {
                anyhow::bail!("Skill source or installed skill not found: {source}");
            }

            let trusted_skill_roots =
                resolve_trusted_skill_roots(workspace_dir, &config.skills.trusted_skill_roots);
            if let Ok(metadata) = std::fs::symlink_metadata(&target) {
                if metadata.file_type().is_symlink() {
                    enforce_workspace_skill_symlink_trust(&target, &trusted_skill_roots)
                        .with_context(|| {
                            format!(
                                "trusted-symlink policy rejected audit target {}",
                                target.display()
                            )
                        })?;
                }
            }

            let report = audit::audit_skill_directory_with_options(
                &target,
                audit::SkillAuditOptions {
                    allow_scripts: config.skills.allow_scripts,
                },
            )?;
            if report.is_clean() {
                println!(
                    "  {} Skill audit passed for {} ({} files scanned).",
                    console::style("✓").green().bold(),
                    target.display(),
                    report.files_scanned
                );
                return Ok(());
            }

            println!(
                "  {} Skill audit failed for {}",
                console::style("✗").red().bold(),
                target.display()
            );
            for finding in report.findings {
                println!("    - {finding}");
            }
            anyhow::bail!("Skill audit failed.");
        }
        crate::SkillCommands::Install { source } => {
            println!("Installing skill from: {source}");

            init_skills_dir(workspace_dir)?;
            let skills_path = skills_dir(workspace_dir);
            let mut download_policy = load_or_init_skill_download_policy(&skills_path)?;
            let source = source.trim().to_string();
            let resolved_source = resolve_skill_source_alias(&source, &download_policy);
            if resolved_source != source {
                println!("  Using configured alias '{source}' -> {resolved_source}");
            }
            ensure_source_domain_trust(&resolved_source, &mut download_policy, &skills_path)?;

            if is_skills_sh_source(&resolved_source) {
                let (installed_dir, files_scanned) =
                    install_skills_sh_source(&resolved_source, &skills_path).with_context(
                        || format!("failed to install skills.sh skill: {resolved_source}"),
                    )?;
                println!(
                    "  {} Skill installed from skills.sh: {} ({} files scanned)",
                    console::style("✓").green().bold(),
                    installed_dir.display(),
                    files_scanned
                );
            } else if is_git_source(&resolved_source) {
                let (installed_dir, files_scanned) =
                    install_git_skill_source(&resolved_source, &skills_path).with_context(
                        || format!("failed to install git skill source: {resolved_source}"),
                    )?;
                println!(
                    "  {} Skill installed and audited: {} ({} files scanned)",
                    console::style("✓").green().bold(),
                    installed_dir.display(),
                    files_scanned
                );
            } else {
                let (dest, files_scanned) =
                    install_local_skill_source(&resolved_source, &skills_path).with_context(
                        || format!("failed to install local skill source: {resolved_source}"),
                    )?;
                println!(
                    "  {} Skill installed and audited: {} ({} files scanned)",
                    console::style("✓").green().bold(),
                    dest.display(),
                    files_scanned
                );
            }

            println!("  Security audit completed successfully.");
            Ok(())
        }
        crate::SkillCommands::Remove { name } => {
            // Reject path traversal attempts
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                anyhow::bail!("Invalid skill name: {name}");
            }

            let skill_path = skills_dir(workspace_dir).join(&name);

            // Verify the resolved path is actually inside the skills directory
            let canonical_skills = skills_dir(workspace_dir)
                .canonicalize()
                .unwrap_or_else(|_| skills_dir(workspace_dir));
            if let Ok(canonical_skill) = skill_path.canonicalize() {
                if !canonical_skill.starts_with(&canonical_skills) {
                    anyhow::bail!("Skill path escapes skills directory: {name}");
                }
            }

            if !skill_path.exists() {
                anyhow::bail!("Skill not found: {name}");
            }

            std::fs::remove_dir_all(&skill_path)?;
            println!(
                "  {} Skill '{}' removed.",
                console::style("✓").green().bold(),
                name
            );
            Ok(())
        }
        crate::SkillCommands::New { .. } => {
            anyhow::bail!("'skills new' is not yet implemented");
        }
        crate::SkillCommands::Test { .. } => {
            anyhow::bail!("'skills test' is not yet implemented");
        }
        crate::SkillCommands::Templates => {
            anyhow::bail!("'skills templates' is not yet implemented");
        }
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn open_skills_env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn load_empty_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skill_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "test-skill"
description = "A test skill"
version = "1.0.0"
tags = ["test"]

[[tools]]
name = "hello"
description = "Says hello"
kind = "shell"
command = "echo hello"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].tools.len(), 1);
        assert_eq!(skills[0].tools[0].name, "hello");
    }

    #[test]
    fn load_skill_from_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("md-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.md"),
            "# My Skill\nThis skill does cool things.\n",
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "md-skill");
        assert!(skills[0].description.contains("cool things"));
    }

    #[test]
    fn skills_to_prompt_empty() {
        let prompt = skills_to_prompt(&[], Path::new("/tmp"));
        assert!(prompt.is_empty());
    }

    #[test]
    fn skills_to_prompt_with_skills() {
        let skills = vec![Skill {
            name: "test".to_string(),
            description: "A test".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Do the thing.".to_string()],
            location: None,
            always: false,
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<instruction>Do the thing.</instruction>"));
    }

    #[test]
    fn skills_to_prompt_compact_mode_omits_instructions_and_tools() {
        let skills = vec![Skill {
            name: "test".to_string(),
            description: "A test".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "run".to_string(),
                description: "Run task".to_string(),
                kind: "shell".to_string(),
                command: "echo hi".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec!["Do the thing.".to_string()],
            location: Some(PathBuf::from("/tmp/workspace/skills/test/SKILL.md")),
            always: false,
        }];
        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/tmp/workspace"),
            crate::config::SkillsPromptInjectionMode::Compact,
        );

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<location>skills/test/SKILL.md</location>"));
        assert!(prompt.contains("loaded on demand"));
        assert!(!prompt.contains("<instructions>"));
        assert!(!prompt.contains("<instruction>Do the thing.</instruction>"));
        assert!(!prompt.contains("<tools>"));
    }

    #[test]
    fn skills_to_prompt_compact_mode_includes_always_skill_instructions_and_tools() {
        let skills = vec![Skill {
            name: "always-skill".to_string(),
            description: "Must always inject".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "run".to_string(),
                description: "Run task".to_string(),
                kind: "shell".to_string(),
                command: "echo hi".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec!["Do the thing every time.".to_string()],
            location: Some(PathBuf::from("/tmp/workspace/skills/always-skill/SKILL.md")),
            always: true,
        }];
        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/tmp/workspace"),
            crate::config::SkillsPromptInjectionMode::Compact,
        );

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>always-skill</name>"));
        assert!(prompt.contains("<instruction>Do the thing every time.</instruction>"));
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("<name>run</name>"));
        assert!(prompt.contains("<kind>shell</kind>"));
    }

    #[test]
    fn load_skill_md_front_matter_overrides_metadata_and_description() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("fm-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        fs::write(
            &skill_md,
            r#"---
name: "overridden-name"
version: "2.1.3"
author: "alice"
description: "Front-matter description"
always: true
---
# Heading
Body text that should be included.
"#,
        )
        .unwrap();

        let skill = load_skill_md(&skill_md, &skill_dir).unwrap();
        assert_eq!(skill.name, "overridden-name");
        assert_eq!(skill.version, "2.1.3");
        assert_eq!(skill.author.as_deref(), Some("alice"));
        assert_eq!(skill.description, "Front-matter description");
        assert!(skill.always);
        assert_eq!(skill.prompts.len(), 1);
        assert!(!skill.prompts[0].contains("name: \"overridden-name\""));
        assert!(skill.prompts[0].contains("# Heading"));
    }

    #[test]
    fn init_skills_creates_readme() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        assert!(dir.path().join("skills").join("README.md").exists());
        assert!(dir
            .path()
            .join("skills")
            .join("find-skills")
            .join("SKILL.md")
            .exists());
        assert!(dir
            .path()
            .join("skills")
            .join("skill-creator")
            .join("SKILL.md")
            .exists());
        assert!(dir
            .path()
            .join("skills")
            .join(".download-policy.toml")
            .exists());
    }

    #[test]
    fn init_skills_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        init_skills_dir(dir.path()).unwrap(); // second call should not fail
        assert!(dir.path().join("skills").join("README.md").exists());
        assert!(dir
            .path()
            .join("skills")
            .join("find-skills")
            .join("SKILL.md")
            .exists());
        assert!(dir
            .path()
            .join("skills")
            .join("skill-creator")
            .join("SKILL.md")
            .exists());
    }

    #[test]
    fn load_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nonexistent");
        let skills = load_skills(&fake);
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_files_in_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // A file, not a directory — should be ignored
        fs::write(skills_dir.join("not-a-skill.txt"), "hello").unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_dir_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let empty_skill = skills_dir.join("empty-skill");
        fs::create_dir_all(&empty_skill).unwrap();
        // Directory exists but no SKILL.toml or SKILL.md
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_multiple_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");

        for name in ["alpha", "beta", "gamma"] {
            let skill_dir = skills_dir.join(name);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\nSkill {name} description.\n"),
            )
            .unwrap();
        }

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn toml_skill_with_multiple_tools() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("multi-tool");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "multi-tool"
description = "Has many tools"
version = "2.0.0"
author = "tester"
tags = ["automation", "devops"]

[[tools]]
name = "build"
description = "Build the project"
kind = "shell"
command = "cargo build"

[[tools]]
name = "test"
description = "Run tests"
kind = "shell"
command = "cargo test"

[[tools]]
name = "deploy"
description = "Deploy via HTTP"
kind = "http"
command = "https://api.example.com/deploy"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        let s = &skills[0];
        assert_eq!(s.name, "multi-tool");
        assert_eq!(s.version, "2.0.0");
        assert_eq!(s.author.as_deref(), Some("tester"));
        assert_eq!(s.tags, vec!["automation", "devops"]);
        assert_eq!(s.tools.len(), 3);
        assert_eq!(s.tools[0].name, "build");
        assert_eq!(s.tools[1].kind, "shell");
        assert_eq!(s.tools[2].kind, "http");
    }

    #[test]
    fn toml_skill_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("minimal");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "minimal"
description = "Bare minimum"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].version, "0.1.0"); // default version
        assert!(skills[0].author.is_none());
        assert!(skills[0].tags.is_empty());
        assert!(skills[0].tools.is_empty());
    }

    #[test]
    fn toml_skill_invalid_syntax_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("broken");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.toml"), "this is not valid toml {{{{").unwrap();

        let skills = load_skills(dir.path());
        assert!(skills.is_empty()); // broken skill is skipped
    }

    #[test]
    fn md_skill_heading_only() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("heading-only");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.md"), "# Just a Heading\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "No description");
    }

    #[test]
    fn skills_to_prompt_includes_tools() {
        let skills = vec![Skill {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "get_weather".to_string(),
                description: "Fetch forecast".to_string(),
                kind: "shell".to_string(),
                command: "curl wttr.in".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec![],
            location: None,
            always: false,
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("weather"));
        assert!(prompt.contains("<name>get_weather</name>"));
        assert!(prompt.contains("<description>Fetch forecast</description>"));
        assert!(prompt.contains("<kind>shell</kind>"));
    }

    #[test]
    fn skills_to_prompt_escapes_xml_content() {
        let skills = vec![Skill {
            name: "xml<skill>".to_string(),
            description: "A & B".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Use <tool> & check \"quotes\".".to_string()],
            location: None,
            always: false,
        }];

        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<name>xml&lt;skill&gt;</name>"));
        assert!(prompt.contains("<description>A &amp; B</description>"));
        assert!(prompt.contains(
            "<instruction>Use &lt;tool&gt; &amp; check &quot;quotes&quot;.</instruction>"
        ));
    }

    #[test]
    fn git_source_detection_accepts_remote_protocols_and_scp_style() {
        let sources = [
            "https://github.com/some-org/some-skill.git",
            "http://github.com/some-org/some-skill.git",
            "ssh://git@github.com/some-org/some-skill.git",
            "git://github.com/some-org/some-skill.git",
            "git@github.com:some-org/some-skill.git",
            "git@localhost:skills/some-skill.git",
        ];

        for source in sources {
            assert!(
                is_git_source(source),
                "expected git source detection for '{source}'"
            );
        }
    }

    #[test]
    fn git_source_detection_rejects_local_paths_and_invalid_inputs() {
        let sources = [
            "./skills/local-skill",
            "/tmp/skills/local-skill",
            "C:\\skills\\local-skill",
            "git@github.com",
            "ssh://",
            "not-a-url",
            "dir/git@github.com:org/repo.git",
        ];

        for source in sources {
            assert!(
                !is_git_source(source),
                "expected local/invalid source detection for '{source}'"
            );
        }
    }

    #[test]
    fn parse_skills_sh_source_accepts_owner_repo_skill_urls() {
        let parsed = parse_skills_sh_source("https://skills.sh/vercel-labs/skills/find-skills")
            .expect("should parse skills.sh source");
        assert_eq!(parsed.owner, "vercel-labs");
        assert_eq!(parsed.repo, "skills");
        assert_eq!(parsed.skill, "find-skills");

        let parsed_with_trailing =
            parse_skills_sh_source("https://skills.sh/anthropics/skills/skill-creator/")
                .expect("should parse trailing slash");
        assert_eq!(parsed_with_trailing.owner, "anthropics");
        assert_eq!(parsed_with_trailing.repo, "skills");
        assert_eq!(parsed_with_trailing.skill, "skill-creator");
    }

    #[test]
    fn parse_skills_sh_source_rejects_invalid_urls() {
        assert!(parse_skills_sh_source("https://skills.sh/vercel-labs/skills").is_none());
        assert!(
            parse_skills_sh_source("https://example.com/vercel-labs/skills/find-skills").is_none()
        );
        assert!(parse_skills_sh_source("skills.sh/vercel-labs/skills/find-skills").is_none());
    }

    #[test]
    fn default_download_policy_contains_required_preloaded_sources() {
        let policy = SkillDownloadPolicy::default();
        assert_eq!(
            policy.aliases.get("find-skills"),
            Some(&"https://skills.sh/vercel-labs/skills/find-skills".to_string())
        );
        assert_eq!(
            policy.aliases.get("skill-creator"),
            Some(&"https://skills.sh/anthropics/skills/skill-creator".to_string())
        );
    }

    #[test]
    fn resolve_skill_source_alias_prefers_user_and_default_aliases() {
        let mut policy = SkillDownloadPolicy::default();
        policy.aliases.insert(
            "custom".to_string(),
            "https://skills.sh/acme/skills/custom".to_string(),
        );

        assert_eq!(
            resolve_skill_source_alias("custom", &policy),
            "https://skills.sh/acme/skills/custom".to_string()
        );
        assert_eq!(
            resolve_skill_source_alias("find-skills", &policy),
            "https://skills.sh/vercel-labs/skills/find-skills".to_string()
        );
        assert_eq!(
            resolve_skill_source_alias("https://example.com/skill.zip", &policy),
            "https://example.com/skill.zip".to_string()
        );
    }

    #[test]
    fn host_matches_trusted_domain_supports_subdomains() {
        assert!(host_matches_trusted_domain("skills.sh", "skills.sh"));
        assert!(host_matches_trusted_domain("cdn.skills.sh", "skills.sh"));
        assert!(!host_matches_trusted_domain("evilskills.sh", "skills.sh"));
    }

    #[test]
    fn normalize_skills_sh_dir_name_preserves_hyphens() {
        assert_eq!(normalize_skills_sh_dir_name("find-skills"), "find-skills");
        assert_eq!(
            normalize_skills_sh_dir_name("Skill-Creator_2"),
            "skill-creator_2"
        );
    }

    #[test]
    fn skills_dir_path() {
        let base = std::path::Path::new("/home/user/.zeroclaw");
        let dir = skills_dir(base);
        assert_eq!(dir, PathBuf::from("/home/user/.zeroclaw/skills"));
    }

    #[test]
    fn toml_prefers_over_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("dual");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname = \"from-toml\"\ndescription = \"TOML wins\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("SKILL.md"), "# From MD\nMD description\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "from-toml"); // TOML takes priority
    }

    #[test]
    fn open_skills_enabled_resolution_prefers_env_then_config_then_default_false() {
        assert!(!open_skills_enabled_from_sources(None, None));
        assert!(open_skills_enabled_from_sources(Some(true), None));
        assert!(!open_skills_enabled_from_sources(Some(true), Some("0")));
        assert!(open_skills_enabled_from_sources(Some(false), Some("yes")));
        // Invalid env values should fall back to config.
        assert!(open_skills_enabled_from_sources(
            Some(true),
            Some("invalid")
        ));
        assert!(!open_skills_enabled_from_sources(
            Some(false),
            Some("invalid")
        ));
    }

    #[test]
    fn resolve_open_skills_dir_resolution_prefers_env_then_config_then_home() {
        let home = Path::new("/tmp/home-dir");
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("/tmp/env-skills"),
                Some("/tmp/config"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/env-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("   "),
                Some("/tmp/config-skills"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/config-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(None, None, Some(home)),
            Some(PathBuf::from("/tmp/home-dir/open-skills"))
        );
        assert_eq!(resolve_open_skills_dir_from_sources(None, None, None), None);
    }

    #[test]
    fn load_skills_with_config_reads_open_skills_dir_without_network() {
        let _env_guard = open_skills_env_lock().lock().unwrap();
        let _enabled_guard = EnvVarGuard::unset("ZEROCLAW_OPEN_SKILLS_ENABLED");
        let _dir_guard = EnvVarGuard::unset("ZEROCLAW_OPEN_SKILLS_DIR");

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        let open_skills_dir = dir.path().join("open-skills-local");
        fs::create_dir_all(open_skills_dir.join("skills/http_request")).unwrap();
        fs::write(open_skills_dir.join("README.md"), "# open skills\n").unwrap();
        fs::write(
            open_skills_dir.join("CONTRIBUTING.md"),
            "# contribution guide\n",
        )
        .unwrap();
        fs::write(
            open_skills_dir.join("skills/http_request/SKILL.md"),
            "# HTTP request\nFetch API responses.\n",
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = true;
        config.skills.open_skills_dir = Some(open_skills_dir.to_string_lossy().to_string());

        let skills = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "http_request");
        assert_ne!(skills[0].name, "CONTRIBUTING");
    }
}

#[cfg(test)]
mod symlink_tests;
