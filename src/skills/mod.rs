use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

mod audit;

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
    load_skills_with_open_skills_config(workspace_dir, None, None)
}

/// Load skills using runtime config values (preferred at runtime).
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    )
}

fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Vec<Skill> {
    let mut skills = Vec::new();

    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        skills.extend(load_open_skills(&open_skills_dir));
    }

    skills.extend(load_workspace_skills(workspace_dir));
    skills
}

fn load_workspace_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir)
}

fn load_skills_from_directory(skills_dir: &Path) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
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
        return load_skills_from_directory(&nested_skills_dir);
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
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
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
    })
}

fn extract_description(content: &str) -> String {
    content
        .lines()
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
             Skill instructions are loaded on demand: read the skill file in `location` only when needed.\n\n\
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

        if matches!(mode, crate::config::SkillsPromptInjectionMode::Full) {
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

fn download_policy_path(skills_path: &Path) -> PathBuf {
    skills_path.join(SKILL_DOWNLOAD_POLICY_FILE)
}

fn normalize_domain_entry(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return s;
    }
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest.to_string();
    }
    s = s
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    s = s
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .to_string();
    if let Some((host, _port)) = s.split_once(':') {
        return host.to_string();
    }
    s
}

fn normalize_domain_list(entries: &mut Vec<String>) {
    let mut normalized = entries
        .iter()
        .map(|entry| normalize_domain_entry(entry))
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    *entries = normalized;
}

fn host_matches_trusted_domain(host: &str, trusted_domain: &str) -> bool {
    let host = normalize_domain_entry(host);
    let trusted = normalize_domain_entry(trusted_domain);
    if host.is_empty() || trusted.is_empty() {
        return false;
    }
    host == trusted || host.ends_with(&format!(".{trusted}"))
}

fn host_matches_any_domain(host: &str, entries: &[String]) -> bool {
    entries
        .iter()
        .any(|entry| host_matches_trusted_domain(host, entry))
}

fn extract_link_host(url: &str) -> Option<String> {
    let trimmed = url.strip_prefix("zip:").unwrap_or(url);
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
        .or_else(|| trimmed.strip_prefix("git://"))?;
    let host_part = rest.split(&['/', '?', '#'][..]).next().unwrap_or("");
    let host_part = host_part.rsplit('@').next().unwrap_or(host_part);
    let host = host_part.split(':').next().unwrap_or("");
    let normalized = normalize_domain_entry(host);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn source_urls_for_trust_check(source: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |url: String| {
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    };

    if source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("ssh://")
        || source.starts_with("git://")
    {
        push_unique(source.to_string());
    }

    if let Some(skills_source) = parse_skills_sh_source(source) {
        push_unique(skills_source.github_repo_url());
    }

    urls
}

fn load_or_init_skill_download_policy(skills_path: &Path) -> Result<SkillDownloadPolicy> {
    let path = download_policy_path(skills_path);
    if !path.exists() {
        let policy = SkillDownloadPolicy::default();
        save_skill_download_policy(skills_path, &policy)?;
        return Ok(policy);
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read skill download policy {}", path.display()))?;
    let mut policy: SkillDownloadPolicy = toml::from_str(&raw).unwrap_or_default();
    let mut policy_changed = false;
    for (alias, source) in default_preloaded_skill_aliases() {
        if let std::collections::btree_map::Entry::Vacant(entry) = policy.aliases.entry(alias) {
            entry.insert(source);
            policy_changed = true;
        }
    }
    let before_trusted = policy.trusted_domains.clone();
    let before_blocked = policy.blocked_domains.clone();
    normalize_domain_list(&mut policy.trusted_domains);
    normalize_domain_list(&mut policy.blocked_domains);
    if before_trusted != policy.trusted_domains || before_blocked != policy.blocked_domains {
        policy_changed = true;
    }
    if policy_changed {
        save_skill_download_policy(skills_path, &policy)?;
    }
    Ok(policy)
}

fn save_skill_download_policy(skills_path: &Path, policy: &SkillDownloadPolicy) -> Result<()> {
    let mut to_save = policy.clone();
    normalize_domain_list(&mut to_save.trusted_domains);
    normalize_domain_list(&mut to_save.blocked_domains);
    let serialized =
        toml::to_string_pretty(&to_save).context("failed to serialize skill download policy")?;
    let path = download_policy_path(skills_path);
    std::fs::write(&path, serialized)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn resolve_skill_source_alias(source: &str, policy: &SkillDownloadPolicy) -> String {
    policy
        .aliases
        .get(source.trim())
        .cloned()
        .unwrap_or_else(|| source.to_string())
}

fn ensure_source_domain_trust(
    source: &str,
    policy: &mut SkillDownloadPolicy,
    skills_path: &Path,
) -> Result<()> {
    let urls = source_urls_for_trust_check(source);
    if urls.is_empty() {
        return Ok(());
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut policy_changed = false;

    for url in urls {
        let Some(host) = extract_link_host(&url) else {
            continue;
        };

        if host_matches_any_domain(&host, &policy.blocked_domains) {
            anyhow::bail!(
                "Domain '{host}' is explicitly blocked for skill downloads. \
                 Remove it from {}/{} to allow download.",
                skills_path.display(),
                SKILL_DOWNLOAD_POLICY_FILE
            );
        }
        if host_matches_any_domain(&host, &policy.trusted_domains) {
            continue;
        }

        if !interactive {
            anyhow::bail!(
                "Refusing to download skill from untrusted domain '{host}' in non-interactive mode. \
                 Re-run interactively to approve, or add the domain to trusted_domains in {}/{}.",
                skills_path.display(),
                SKILL_DOWNLOAD_POLICY_FILE
            );
        }

        let trust = dialoguer::Confirm::new()
            .with_prompt(format!(
                "First time downloading a skill from '{host}'. Trust this domain for future downloads?"
            ))
            .default(false)
            .interact()
            .context("failed to read domain trust confirmation")?;

        if trust {
            policy.trusted_domains.push(host);
            policy_changed = true;
            continue;
        }

        policy.blocked_domains.push(host);
        save_skill_download_policy(skills_path, policy)?;
        anyhow::bail!("Skill download canceled because the source domain was not trusted.");
    }

    if policy_changed {
        save_skill_download_policy(skills_path, policy)?;
    }

    Ok(())
}

fn ensure_builtin_preloaded_skills(skills_path: &Path) -> Result<()> {
    for builtin in BUILTIN_PRELOADED_SKILLS {
        let skill_dir = skills_path.join(builtin.dir_name);
        if skill_dir.exists() {
            continue;
        }

        std::fs::create_dir_all(&skill_dir)
            .with_context(|| format!("failed to create {}", skill_dir.display()))?;
        std::fs::write(skill_dir.join("SKILL.md"), builtin.markdown).with_context(|| {
            format!(
                "failed to write preloaded skill {}",
                skill_dir.join("SKILL.md").display()
            )
        })?;
        let meta = serde_json::json!({
            "slug": builtin.dir_name,
            "version": "preloaded",
            "source": builtin.source_url
        });
        std::fs::write(
            skill_dir.join("_meta.json"),
            serde_json::to_vec_pretty(&meta)?,
        )
        .with_context(|| format!("failed to write {}", skill_dir.join("_meta.json").display()))?;
    }
    Ok(())
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

            let report = audit::audit_skill_directory(&target)?;
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
