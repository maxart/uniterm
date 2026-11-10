//! Versioned provider-owned process, screen, and log evidence manifests.
//! The generic reconciler consumes compiled manifests without branching on an
//! agent id. Sources are layered as local override, verified cache,
//! last-known-good cache, then bundled data. All filesystem work and reload
//! watching live on the agent runtime.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use uniterm_core::AgentStatus;
use uniterm_proto::{DetectionCapability, DetectionProvenance, DetectionSource};

mod amp;
mod approval;
mod claude;
mod codex;
mod copilot;
mod cursor;
mod droid;
mod gemini;
mod grok;
mod hermes;
mod kilo;
mod kimi;
mod kiro;
mod omp;
mod opencode;
mod pi;
mod qoder;
mod qodercli;
pub(crate) use approval::Approval;
pub(crate) mod sessions;

/// Recognize an actionable permission using bundled provider rules only.
pub(crate) fn permission_approval(
    provider: &str,
    term: &crate::terminal::Terminal,
) -> Option<Approval> {
    let manifest = launch_manifest(provider)?;
    let screen = term.evidence_text(48);
    if manifest.id == "opencode" {
        if let Some(choice) = opencode::approval(term, &screen) {
            return Some(choice);
        }
    }
    approval::recognize(provider, &screen)
}

/// Native transcript parsing stays behind provider dispatch.
fn session_identity(provider: &str, line: &str) -> Option<sessions::Identity> {
    match provider {
        "codex" => codex::session_identity(line),
        "pi" | "omp" => pi::session_identity(line),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn native_process_matches(provider: &str, command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    BUILT_INS
        .iter()
        .find(|manifest| manifest.id == provider)
        .is_some_and(|manifest| {
            manifest
                .executables
                .iter()
                .any(|executable| command_contains_executable(&command, executable))
        })
}

fn supports_native_session(provider: &str) -> bool {
    matches!(provider, "codex" | "pi" | "omp")
}

fn default_session_root(provider: &str, pid: Option<i32>) -> Option<PathBuf> {
    match provider {
        "codex" => codex::session_root(pid),
        _ => None,
    }
}

fn known_session_path(provider: &str, root: &Path, id: &str) -> Option<PathBuf> {
    match provider {
        "codex" => codex::session_path(root, id),
        _ => None,
    }
}

fn session_watch_root(provider: &str, path: &Path) -> Option<PathBuf> {
    match provider {
        "codex" => codex::session_watch_root(path),
        _ => None,
    }
}

const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_LAST_GOOD_BYTES: u64 = MAX_MANIFEST_BYTES * 2 + 4096;
const MAX_PROVIDERS: usize = 128;
const MAX_RULES: usize = 512;
const MAX_ALIASES: usize = 64;
const MAX_PATTERN_BYTES: usize = 512;
const BUNDLED_PRECEDENCE: u8 = 10;
const LAST_GOOD_PRECEDENCE: u8 = 20;
const CACHE_PRECEDENCE: u8 = 30;
const LOCAL_PRECEDENCE: u8 = 40;

/// Where a screen rule looks. The window title is a first-class region
/// because most agent TUIs put their busy spinner or an "action required"
/// marker there, well away from anything the user can type.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleRegion {
    /// The bottom rows of the live grid.
    #[default]
    Bottom,
    /// The OSC 0/2 title the application last set.
    Title,
}

/// How a pattern must sit in a line. Working and Tool rules are anchored so
/// text the user types into the agent's input box, which is indented behind a
/// prompt marker, can never impersonate the agent's own activity line.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleAnchor {
    /// Substring anywhere in the region.
    #[default]
    Anywhere,
    /// A line, after leading whitespace, begins with the pattern.
    LineStart,
    /// A line begins with a spinner glyph, then whitespace, then the pattern
    /// (an empty pattern accepts any spinner line).
    SpinnerLine,
    /// Braille or circle animation followed by whitespace. Static star title
    /// markers are deliberately excluded, even when they resemble a spinner.
    AnimatedTitle,
    /// A spinner-led activity phrase containing an ellipsis. Completed turn
    /// summaries such as "Worked for 2m" must not keep an agent busy.
    SpinnerActivity,
    /// Animated activity line ending in a provider-owned stop control.
    SpinnerSuffix,
}

#[derive(Clone, Copy)]
pub struct Rule {
    pub status: AgentStatus,
    pub patterns: &'static [&'static str],
    pub anchor: RuleAnchor,
    pub region: RuleRegion,
    /// Highest matching priority wins; declaration order breaks ties.
    pub priority: u8,
}

/// Priority for a manifest rule that does not state one.
pub const DEFAULT_RULE_PRIORITY: u8 = 50;

fn default_rule_priority() -> u8 {
    DEFAULT_RULE_PRIORITY
}

#[derive(Clone, Copy)]
pub struct Manifest {
    pub id: &'static str,
    pub executables: &'static [&'static str],
    pub rules: &'static [Rule],
}

/// Resolve built-in launch aliases at the provider boundary, including CLI renames.
pub(crate) fn launch_manifest(id: &str) -> Option<&'static Manifest> {
    BUILT_INS
        .iter()
        .find(|manifest| manifest.id == id)
        .or_else(|| {
            BUILT_INS
                .iter()
                .filter(|manifest| manifest.executables.contains(&id))
                .min_by_key(|manifest| manifest.id)
        })
}

fn approval_dialogs(id: &str) -> &'static [approval::Dialog] {
    match id {
        "codex" => codex::APPROVAL_DIALOGS,
        "claude" => claude::APPROVAL_DIALOGS,
        "grok" => grok::APPROVAL_DIALOGS,
        "pi" | "omp" => pi::APPROVAL_DIALOGS,
        _ => &[],
    }
}

/// Provider-owned interactive launch contract. Native defaults remain untouched.
#[derive(Clone, Copy)]
pub(crate) struct LaunchSpec {
    pub prefix: &'static [&'static str],
    pub prompt_flag: Option<&'static str>,
    pub model_flag: Option<&'static str>,
    pub profile_flag: Option<&'static str>,
}

pub(crate) fn launch_spec(id: &str) -> LaunchSpec {
    match id {
        "claude" => claude::LAUNCH,
        "codex" => codex::LAUNCH,
        "opencode" => opencode::LAUNCH,
        "gemini" => gemini::LAUNCH,
        "grok" => grok::LAUNCH,
        "kiro" => kiro::LAUNCH,
        "cursor" => cursor::LAUNCH,
        "pi" => pi::LAUNCH,
        _ => LaunchSpec {
            prefix: &[],
            prompt_flag: None,
            model_flag: None,
            profile_flag: None,
        },
    }
}

/// Provider-owned native resume argv for a session the provider reported, used
/// when its hooks recorded no resume command: the CLI's own fixed resume flag
/// followed by the session id. `None` for providers without a verified resume
/// flag, and for an id that is not a plain token (it must not read as a flag).
pub(crate) fn resume_argv(id: &str, session_id: &str) -> Option<Vec<String>> {
    let prefix = match id {
        "claude" => claude::RESUME,
        "codex" => codex::RESUME,
        "cursor" => cursor::RESUME,
        "opencode" => opencode::RESUME,
        "copilot" => copilot::RESUME,
        "amp" => amp::RESUME,
        "droid" => droid::RESUME,
        "gemini" => gemini::RESUME,
        "grok" => grok::RESUME,
        "hermes" => hermes::RESUME,
        "kilo" => kilo::RESUME,
        "kimi" => kimi::RESUME,
        "kiro" => kiro::RESUME,
        "omp" => omp::RESUME,
        "pi" => pi::RESUME,
        "qoder" => qoder::RESUME,
        "qodercli" => qodercli::RESUME,
        _ => return None,
    };
    let plain = !session_id.is_empty()
        && session_id.len() <= 200
        && !session_id.starts_with('-')
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    plain.then(|| {
        prefix
            .iter()
            .map(|part| part.to_string())
            .chain([session_id.to_string()])
            .collect()
    })
}

/// Fill a saved profile's empty resume command from its session id (`resume_argv`).
pub(crate) fn complete_resume(profile: &mut crate::persist::AgentLaunchSnap) {
    if !profile.resume_command.is_empty() {
        return;
    }
    if let Some(argv) = profile
        .session_id
        .as_deref()
        .and_then(|session| resume_argv(&profile.provider, session))
    {
        profile.resume_command = argv;
    }
}

/// Provider-owned fresh launch for a saved agent that never reported a session
/// to resume: its primary executable with no arguments.
pub(crate) fn fresh_argv(id: &str) -> Option<Vec<String>> {
    BUILT_INS
        .iter()
        .find(|manifest| manifest.id == id)
        .and_then(|manifest| manifest.executables.first())
        .map(|executable| vec![executable.to_string()])
}

/// Construct argv without passing prompt text through an option parser as flags.
pub(crate) fn launch_argv(
    id: &str,
    command: &str,
    prompt: &str,
    model: Option<&str>,
    profile: Option<&str>,
) -> Result<Vec<String>, String> {
    let spec = launch_spec(id);
    let mut argv = vec![command.to_string()];
    argv.extend(spec.prefix.iter().map(|s| s.to_string()));
    for (value, flag, label) in [
        (model, spec.model_flag, "model"),
        (profile, spec.profile_flag, "profile"),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            let flag =
                flag.ok_or_else(|| format!("{id} does not support a {label} launch override"))?;
            if value.contains('\0') {
                return Err(format!("{label} must not contain NUL bytes"));
            }
            argv.push(format!("{flag}={value}"));
        }
    }
    if let Some(flag) = spec.prompt_flag {
        argv.push(format!("{flag}={prompt}"));
    } else {
        if launch_manifest(id).is_some() {
            argv.push("--".into());
        }
        argv.push(prompt.to_string());
    }
    Ok(argv)
}

const BUILT_INS: &[Manifest] = &[
    claude::MANIFEST,
    codex::MANIFEST,
    opencode::MANIFEST,
    gemini::MANIFEST,
    grok::MANIFEST,
    kiro::MANIFEST,
    cursor::MANIFEST,
    pi::MANIFEST,
    copilot::MANIFEST,
    kimi::MANIFEST,
    droid::MANIFEST,
    amp::MANIFEST,
    hermes::MANIFEST,
    kilo::MANIFEST,
    qodercli::MANIFEST,
    qoder::MANIFEST,
    omp::MANIFEST,
];

/// One versioned manifest file. A source is rejected as a unit when any
/// provider or rule is invalid, so a partially accepted update cannot create
/// surprising precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDocument {
    pub schema_version: u32,
    pub manifest_version: String,
    pub providers: Vec<ProviderDefinition>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LastGoodEnvelope {
    sha256: String,
    manifest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    pub id: String,
    /// Bare executable names and package path components that identify the
    /// provider's foreground process.
    #[serde(default, alias = "executables")]
    pub executable_aliases: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<ManifestCapability>,
    #[serde(default)]
    pub log_path: Option<String>,
    #[serde(default)]
    pub rules: Vec<ManifestRule>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ManifestCapability {
    Process,
    Screen,
    Log,
    Connector,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRule {
    pub id: String,
    pub evidence: ManifestEvidence,
    pub status: ManifestStatus,
    pub pattern: String,
    pub confidence: u8,
    #[serde(default)]
    pub dwell_ms: Option<u64>,
    #[serde(default)]
    pub anchor: RuleAnchor,
    #[serde(default)]
    pub region: RuleRegion,
    #[serde(default = "default_rule_priority")]
    pub priority: u8,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestEvidence {
    Screen,
    Log,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestStatus {
    Starting,
    Working,
    Tool,
    Permission,
    Question,
    Idle,
    Error,
    Exited,
}

impl From<ManifestStatus> for AgentStatus {
    fn from(status: ManifestStatus) -> Self {
        match status {
            ManifestStatus::Starting => AgentStatus::Starting,
            ManifestStatus::Working => AgentStatus::Working,
            ManifestStatus::Tool => AgentStatus::Tool,
            ManifestStatus::Permission => AgentStatus::Permission,
            ManifestStatus::Question => AgentStatus::Question,
            ManifestStatus::Idle => AgentStatus::Idle,
            ManifestStatus::Error => AgentStatus::Error,
            ManifestStatus::Exited => AgentStatus::Exited,
        }
    }
}

#[derive(Clone)]
struct CompiledRule {
    id: String,
    evidence: ManifestEvidence,
    status: AgentStatus,
    pattern: String,
    lower_pattern: String,
    confidence: u8,
    dwell_ms: Option<u64>,
    anchor: RuleAnchor,
    region: RuleRegion,
    priority: u8,
}

#[derive(Clone)]
struct CompiledManifest {
    id: String,
    executables: Vec<String>,
    log_path: Option<String>,
    rules: Vec<CompiledRule>,
    capabilities: Vec<DetectionCapability>,
    source: DetectionSource,
    version: String,
    precedence: u8,
}

/// Provider catalog snapshotted by evidence workers. Reload replaces the
/// runtime-owned `Arc<Catalog>`; neither the mio core nor workers share mutable
/// manifest state.
#[derive(Clone)]
pub struct Catalog {
    manifests: Vec<CompiledManifest>,
    diagnostics: Vec<String>,
    activation_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub agent: Option<String>,
    pub status: Option<AgentStatus>,
    pub evidence: String,
    pub provenance: DetectionProvenance,
}

impl Match {
    /// Attach the exact foreground invocation that produced this observation.
    pub fn with_invocation(mut self, pid: Option<i32>) -> Self {
        self.provenance.invocation_pid = pid;
        self
    }
}

/// Successful offline validation summary printed by the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationSummary {
    pub manifest_version: String,
    pub providers: usize,
    pub rules: usize,
}

impl Catalog {
    pub fn load() -> Catalog {
        let local = config_path();
        let cache = cache_path();
        let last_good = last_good_path();
        Self::load_from_paths(local.as_deref(), cache.as_deref(), Some(&last_good))
    }

    /// Load an explicit source set so evidence tests can avoid machine configuration.
    pub(crate) fn load_from_paths(
        local: Option<&Path>,
        cache: Option<&Path>,
        last_good: Option<&Path>,
    ) -> Catalog {
        let mut selected: HashMap<String, CompiledManifest> = bundled()
            .into_iter()
            .map(|manifest| (manifest.id.clone(), manifest))
            .collect();
        let mut diagnostics = Vec::new();
        let mut activation_valid = true;

        let cached = cache.and_then(|path| match read_verified(path) {
            Ok(Some((document, bytes))) => {
                if let Some(last_good) = last_good {
                    if let Err(error) = persist_last_good(last_good, &bytes) {
                        diagnostics
                            .push(format!("could not save provider last-known-good: {error}"));
                    }
                }
                Some((document, DetectionSource::VerifiedCache, CACHE_PRECEDENCE))
            }
            Ok(None) => None,
            Err(error) => {
                diagnostics.push(format!("verified provider cache rejected: {error}"));
                None
            }
        });
        let cached = cached.or_else(|| {
            last_good.and_then(|path| match read_last_good(path) {
                Ok(Some((document, _))) => Some((
                    document,
                    DetectionSource::LastKnownGood,
                    LAST_GOOD_PRECEDENCE,
                )),
                Ok(None) => None,
                Err(error) => {
                    diagnostics.push(format!("provider last-known-good rejected: {error}"));
                    None
                }
            })
        });
        if let Some((document, source, precedence)) = cached {
            insert_document(&mut selected, document, source, precedence);
        }

        if let Some(path) = local {
            match read_document(path) {
                Ok(Some(document)) => insert_document(
                    &mut selected,
                    document,
                    DetectionSource::LocalOverride,
                    LOCAL_PRECEDENCE,
                ),
                Ok(None) => {}
                Err(error) => {
                    activation_valid = false;
                    diagnostics.push(format!("local provider manifest rejected: {error}"))
                }
            }
        }

        let mut manifests: Vec<_> = selected.into_values().collect();
        manifests.sort_by(|left, right| {
            right
                .precedence
                .cmp(&left.precedence)
                .then_with(|| left.id.cmp(&right.id))
        });
        Catalog {
            manifests,
            diagnostics,
            activation_valid,
        }
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Whether this snapshot may replace an active catalog. A malformed local
    /// override retains the prior snapshot instead of silently removing the
    /// user's definitions.
    pub fn activation_valid(&self) -> bool {
        self.activation_valid
    }

    /// Identify an agent from an event-triggered foreground process command.
    pub fn process(&self, command: &str) -> Option<Match> {
        let lower = command.to_ascii_lowercase();
        for manifest in &self.manifests {
            if let Some(executable) = manifest
                .executables
                .iter()
                .find(|exe| command_contains_executable(&lower, exe))
            {
                return Some(Match {
                    agent: Some(manifest.id.clone()),
                    status: None,
                    evidence: format!("foreground process matched {}", safe_evidence(command, 512)),
                    provenance: provenance(
                        manifest,
                        Some(format!("process.{executable}")),
                        Some(70),
                        None,
                    ),
                });
            }
        }
        None
    }

    /// Classify the live bottom-of-grid text and window title of a known
    /// provider.
    ///
    /// Working and blocked states need a positive, anchored match. When no
    /// rule matches, a known agent is idle: raw output volume is never
    /// evidence, so keyboard echo, a repainting footer, or a resize cannot
    /// make an idle agent look busy, and silence is not needed to make a busy
    /// one look idle.
    pub fn screen(&self, agent: &str, tail: &str, title: &str) -> Option<Match> {
        let manifest = self
            .manifests
            .iter()
            .find(|manifest| manifest.id == agent)?;
        let lower_tail = tail.to_ascii_lowercase();
        let lower_title = title.to_ascii_lowercase();
        let mut best: Option<&CompiledRule> = None;
        for rule in &manifest.rules {
            if rule.evidence != ManifestEvidence::Screen {
                continue;
            }
            let text = match rule.region {
                RuleRegion::Bottom => lower_tail.as_str(),
                RuleRegion::Title => lower_title.as_str(),
            };
            if rule_matches(rule, text)
                && best.is_none_or(|current| rule.priority > current.priority)
            {
                best = Some(rule);
            }
        }
        let Some(rule) = best else {
            return Some(Match {
                agent: Some(manifest.id.clone()),
                status: Some(AgentStatus::Idle),
                evidence: "no working or blocked signal on screen".into(),
                provenance: provenance(
                    manifest,
                    Some("screen.idle.default".into()),
                    Some(50),
                    Some(default_dwell_ms(AgentStatus::Idle)),
                ),
            });
        };
        let label = match rule.region {
            RuleRegion::Bottom => "bottom grid",
            RuleRegion::Title => "window title",
        };
        Some(Match {
            agent: Some(manifest.id.clone()),
            status: Some(rule.status),
            evidence: format!("{label} matched {:?}", rule.pattern),
            provenance: provenance(
                manifest,
                Some(rule.id.clone()),
                Some(rule.confidence),
                rule.dwell_ms,
            ),
        })
    }

    /// Classify a configured provider-native log tail. Reads are triggered by
    /// pane output, never by a timer, and execute only in runtime workers.
    pub fn log(&self, agent: &str) -> Option<Match> {
        let manifest = self
            .manifests
            .iter()
            .find(|manifest| manifest.id == agent)?;
        let raw_path = manifest.log_path.as_deref()?;
        let path = expand_home(raw_path)?;
        let tail = read_file_tail(&path, 64 * 1024).ok()?;
        let lower = String::from_utf8_lossy(&tail).to_ascii_lowercase();
        let mut found = match_rules(manifest, ManifestEvidence::Log, &lower, "session log")?;
        found.evidence = format!("{} in {}", found.evidence, path.display());
        Some(found)
    }
}

fn provenance(
    manifest: &CompiledManifest,
    matched_rule: Option<String>,
    confidence: Option<u8>,
    dwell_ms: Option<u64>,
) -> DetectionProvenance {
    DetectionProvenance {
        source: manifest.source,
        manifest_version: Some(manifest.version.clone()),
        matched_rule,
        confidence,
        dwell_ms,
        precedence: manifest.precedence,
        capabilities: manifest.capabilities.clone(),
        evidence_timestamp_ms: now_ms(),
        invocation_pid: None,
    }
}

/// Glyphs agent TUIs animate in front of an activity line or in the title:
/// braille dots, quarter and half circles, hexagons, and the star set.
/// Recognising the class rather than one agent's glyph keeps the matcher
/// provider-neutral.
pub fn is_spinner_glyph(ch: char) -> bool {
    matches!(
        ch,
        '\u{2800}'..='\u{28FF}'
            | '\u{25D0}'..='\u{25D3}'
            | '\u{25DC}'..='\u{25DF}'
            | '\u{25F4}'..='\u{25F7}'
            | '\u{2B21}'
            | '\u{2B22}'
            | '\u{2722}'
            | '\u{2733}'
            | '\u{2736}'
            | '\u{273B}'
            | '\u{273D}'
            | '\u{2726}'
            | '\u{2727}'
    )
}

fn rule_matches(rule: &CompiledRule, lower: &str) -> bool {
    match rule.anchor {
        RuleAnchor::Anywhere => {
            !rule.lower_pattern.is_empty() && lower.contains(&rule.lower_pattern)
        }
        RuleAnchor::LineStart => {
            !rule.lower_pattern.is_empty()
                && lower
                    .lines()
                    .any(|line| line.trim_start().starts_with(&rule.lower_pattern))
        }
        RuleAnchor::SpinnerLine
        | RuleAnchor::AnimatedTitle
        | RuleAnchor::SpinnerActivity
        | RuleAnchor::SpinnerSuffix => lower.lines().any(|line| {
            let mut chars = line.trim_start().chars();
            let glyph = chars.next();
            let spinner = match rule.anchor {
                RuleAnchor::AnimatedTitle | RuleAnchor::SpinnerSuffix => glyph.is_some_and(
                    |ch| matches!(ch, '\u{2800}'..='\u{28ff}' | '\u{25d0}'..='\u{25d3}'),
                ),
                RuleAnchor::SpinnerActivity => {
                    glyph.is_some_and(|ch| is_spinner_glyph(ch) || matches!(ch, '*' | '·'))
                        && (chars.as_str().contains('…') || chars.as_str().contains("..."))
                }
                _ => glyph.is_some_and(is_spinner_glyph),
            };
            spinner
                && chars.as_str().starts_with(char::is_whitespace)
                && if rule.anchor == RuleAnchor::SpinnerSuffix {
                    chars.as_str().trim_end().ends_with(&rule.lower_pattern)
                } else {
                    chars.as_str().trim_start().starts_with(&rule.lower_pattern)
                }
        }),
    }
}

fn match_rules(
    manifest: &CompiledManifest,
    evidence: ManifestEvidence,
    lower: &str,
    label: &str,
) -> Option<Match> {
    let rule = manifest
        .rules
        .iter()
        .filter(|rule| rule.evidence == evidence && rule_matches(rule, lower))
        .max_by_key(|rule| rule.priority)?;
    Some(Match {
        agent: Some(manifest.id.clone()),
        status: Some(rule.status),
        evidence: format!("{label} matched {:?}", rule.pattern),
        provenance: provenance(
            manifest,
            Some(rule.id.clone()),
            Some(rule.confidence),
            rule.dwell_ms,
        ),
    })
}

fn bundled() -> Vec<CompiledManifest> {
    BUILT_INS
        .iter()
        .map(|manifest| {
            let mut rules = Vec::new();
            for (group, rule) in manifest.rules.iter().enumerate() {
                for (index, pattern) in rule.patterns.iter().enumerate() {
                    rules.push(CompiledRule {
                        id: format!("screen.{}.{}.{}", rule.status.label(), group + 1, index + 1),
                        evidence: ManifestEvidence::Screen,
                        status: rule.status,
                        pattern: (*pattern).into(),
                        lower_pattern: pattern.to_ascii_lowercase(),
                        confidence: if rule.status.needs_human() { 90 } else { 75 },
                        dwell_ms: Some(default_dwell_ms(rule.status)),
                        anchor: rule.anchor,
                        region: rule.region,
                        priority: rule.priority,
                    });
                }
            }
            let mut capabilities = vec![DetectionCapability::Process];
            if !rules.is_empty() {
                capabilities.push(DetectionCapability::Screen);
            }
            if crate::connectors::supports(manifest.id) {
                capabilities.push(DetectionCapability::Connector);
            }
            CompiledManifest {
                id: manifest.id.into(),
                executables: manifest
                    .executables
                    .iter()
                    .map(|value| (*value).into())
                    .collect(),
                log_path: None,
                rules,
                capabilities,
                source: DetectionSource::Bundled,
                version: env!("CARGO_PKG_VERSION").into(),
                precedence: BUNDLED_PRECEDENCE,
            }
        })
        .collect()
}

fn insert_document(
    selected: &mut HashMap<String, CompiledManifest>,
    document: ManifestDocument,
    source: DetectionSource,
    precedence: u8,
) {
    for provider in document.providers {
        let id = provider.id.clone();
        selected.insert(
            id.clone(),
            compile_provider(
                provider,
                source,
                document.manifest_version.clone(),
                precedence,
            ),
        );
    }
}

fn compile_provider(
    provider: ProviderDefinition,
    source: DetectionSource,
    version: String,
    precedence: u8,
) -> CompiledManifest {
    let capabilities = provider
        .capabilities
        .iter()
        .copied()
        .map(|capability| match capability {
            ManifestCapability::Process => DetectionCapability::Process,
            ManifestCapability::Screen => DetectionCapability::Screen,
            ManifestCapability::Log => DetectionCapability::Log,
            ManifestCapability::Connector => DetectionCapability::Connector,
        })
        .collect();
    CompiledManifest {
        id: provider.id,
        executables: provider
            .executable_aliases
            .into_iter()
            .map(|value| value.to_ascii_lowercase())
            .collect(),
        log_path: provider.log_path,
        rules: provider
            .rules
            .into_iter()
            .map(|rule| CompiledRule {
                id: rule.id,
                evidence: rule.evidence,
                status: rule.status.into(),
                lower_pattern: rule.pattern.to_ascii_lowercase(),
                pattern: rule.pattern,
                confidence: rule.confidence,
                dwell_ms: rule.dwell_ms,
                anchor: rule.anchor,
                region: rule.region,
                priority: rule.priority,
            })
            .collect(),
        capabilities,
        source,
        version,
        precedence,
    }
}

/// Validate a manifest without installing or activating it.
pub fn validate_file(path: &Path) -> Result<ValidationSummary, String> {
    let document = read_document(path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{} does not exist", path.display()))?;
    Ok(ValidationSummary {
        manifest_version: document.manifest_version,
        providers: document.providers.len(),
        rules: document
            .providers
            .iter()
            .map(|provider| provider.rules.len())
            .sum(),
    })
}

fn read_document(path: &Path) -> Result<Option<ManifestDocument>, String> {
    let bytes = read_bounded(path)?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    parse_document(&bytes).map(Some)
}

fn parse_document(bytes: &[u8]) -> Result<ManifestDocument, String> {
    let document: ManifestDocument =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid JSON: {error}"))?;
    validate_document(&document)?;
    Ok(document)
}

fn validate_document(document: &ManifestDocument) -> Result<(), String> {
    if document.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {}; expected {SCHEMA_VERSION}",
            document.schema_version
        ));
    }
    validate_name("manifest_version", &document.manifest_version, 64)?;
    if document.providers.is_empty() || document.providers.len() > MAX_PROVIDERS {
        return Err(format!(
            "providers must contain 1 to {MAX_PROVIDERS} entries"
        ));
    }
    let mut provider_ids = HashSet::new();
    let mut executable_owners = HashMap::new();
    let mut total_rules = 0usize;
    for provider in &document.providers {
        validate_name("provider id", &provider.id, 64)?;
        if !provider_ids.insert(provider.id.as_str()) {
            return Err(format!("duplicate provider id {:?}", provider.id));
        }
        if provider.executable_aliases.len() > MAX_ALIASES {
            return Err(format!(
                "provider {:?} has more than {MAX_ALIASES} executable aliases",
                provider.id
            ));
        }
        let mut aliases = HashSet::new();
        for alias in &provider.executable_aliases {
            validate_alias(alias)?;
            let normalized = alias.to_ascii_lowercase();
            if !aliases.insert(normalized.clone()) {
                return Err(format!(
                    "provider {:?} repeats alias {alias:?}",
                    provider.id
                ));
            }
            if let Some(owner) = executable_owners.insert(normalized, provider.id.as_str()) {
                return Err(format!(
                    "executable alias {alias:?} is claimed by both {owner:?} and {:?}",
                    provider.id
                ));
            }
        }
        let capabilities: HashSet<_> = provider.capabilities.iter().copied().collect();
        if capabilities.len() != provider.capabilities.len() {
            return Err(format!("provider {:?} repeats a capability", provider.id));
        }
        if capabilities.contains(&ManifestCapability::Connector)
            != crate::connectors::supports(&provider.id)
        {
            return Err(format!(
                "provider {:?} connector capability must match this build's first-party support",
                provider.id
            ));
        }
        if capabilities.contains(&ManifestCapability::Process)
            == provider.executable_aliases.is_empty()
        {
            return Err(format!(
                "provider {:?} process capability must match executable_aliases",
                provider.id
            ));
        }
        if let Some(path) = &provider.log_path {
            validate_text("log_path", path, 4096)?;
            if path.trim().is_empty() {
                return Err(format!("provider {:?} has an empty log_path", provider.id));
            }
        }
        total_rules = total_rules.saturating_add(provider.rules.len());
        if total_rules > MAX_RULES {
            return Err(format!("a manifest may contain at most {MAX_RULES} rules"));
        }
        let mut rule_ids = HashSet::new();
        let mut has_screen = false;
        let mut has_log = false;
        for rule in &provider.rules {
            validate_name("rule id", &rule.id, 96)?;
            if !rule_ids.insert(rule.id.as_str()) {
                return Err(format!(
                    "provider {:?} repeats rule id {:?}",
                    provider.id, rule.id
                ));
            }
            if !(matches!(
                rule.anchor,
                RuleAnchor::SpinnerLine | RuleAnchor::AnimatedTitle | RuleAnchor::SpinnerActivity
            ) && rule.pattern.is_empty())
            {
                validate_pattern(&rule.pattern)?;
            }
            if !(1..=100).contains(&rule.confidence) {
                return Err(format!("rule {:?} confidence must be 1 to 100", rule.id));
            }
            if rule.dwell_ms.is_some_and(|value| value > 60_000) {
                return Err(format!("rule {:?} dwell_ms exceeds 60000", rule.id));
            }
            match rule.evidence {
                ManifestEvidence::Screen => has_screen = true,
                ManifestEvidence::Log => has_log = true,
            }
        }
        if capabilities.contains(&ManifestCapability::Screen) != has_screen {
            return Err(format!(
                "provider {:?} screen capability must match screen rules",
                provider.id
            ));
        }
        if capabilities.contains(&ManifestCapability::Log)
            != (has_log && provider.log_path.is_some())
        {
            return Err(format!(
                "provider {:?} log capability requires log rules and log_path",
                provider.id
            ));
        }
    }
    Ok(())
}

fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty()
        || alias.len() > 128
        || alias.contains('/')
        || alias.contains('\\')
        || alias
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(format!(
            "executable alias {alias:?} must be a bare 1 to 128 byte name"
        ));
    }
    if matches!(
        alias.to_ascii_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "fish" | "node" | "python" | "python3" | "ruby" | "perl" | "java"
    ) {
        return Err(format!(
            "executable alias {alias:?} is an overbroad interpreter or shell"
        ));
    }
    Ok(())
}

fn validate_name(label: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > limit
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(format!(
            "{label} {value:?} must use 1 to {limit} ASCII letters, digits, '.', '_', or '-'"
        ));
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.len() > limit || value.chars().any(char::is_control) {
        return Err(format!(
            "{label} must be at most {limit} bytes with no control characters"
        ));
    }
    Ok(())
}

fn validate_pattern(pattern: &str) -> Result<(), String> {
    validate_text("pattern", pattern, MAX_PATTERN_BYTES)?;
    let trimmed = pattern.trim();
    let meaningful = trimmed.chars().filter(|ch| ch.is_alphanumeric()).count();
    if trimmed.len() < 4 || meaningful < 3 {
        return Err(format!(
            "pattern {pattern:?} is overbroad; use at least 3 letters or digits and 4 bytes"
        ));
    }
    Ok(())
}

fn read_verified(path: &Path) -> Result<Option<(ManifestDocument, Vec<u8>)>, String> {
    let Some(bytes) = read_bounded(path)? else {
        return Ok(None);
    };
    let digest_path = digest_path(path);
    let expected = read_bounded_limit(&digest_path, 1024)?
        .ok_or_else(|| format!("{} does not exist", digest_path.display()))?;
    let expected = std::str::from_utf8(&expected)
        .map_err(|error| format!("{}: {error}", digest_path.display()))?
        .trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "{} is not a SHA-256 hex digest",
            digest_path.display()
        ));
    }
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!("{} digest does not match", path.display()));
    }
    Ok(Some((parse_document(&bytes)?, bytes)))
}

fn read_last_good(path: &Path) -> Result<Option<(ManifestDocument, Vec<u8>)>, String> {
    let Some(envelope_bytes) = read_bounded_limit(path, MAX_LAST_GOOD_BYTES)? else {
        return Ok(None);
    };
    let envelope: LastGoodEnvelope = serde_json::from_slice(&envelope_bytes)
        .map_err(|error| format!("{}: invalid envelope: {error}", path.display()))?;
    let bytes = envelope.manifest.into_bytes();
    if sha256_hex(&bytes) != envelope.sha256 {
        return Err(format!("{} embedded digest does not match", path.display()));
    }
    Ok(Some((parse_document(&bytes)?, bytes)))
}

fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>, String> {
    read_bounded_limit(path, MAX_MANIFEST_BYTES)
}

fn read_bounded_limit(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let length = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    if length > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn persist_last_good(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let manifest = String::from_utf8(bytes.to_vec())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let envelope = LastGoodEnvelope {
        sha256: sha256_hex(bytes),
        manifest,
    };
    let encoded = serde_json::to_vec(&envelope).map_err(std::io::Error::other)?;
    write_private_atomic(path, &encoded)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing parent",
        ));
    };
    crate::persist::ensure_private_dir(parent)?;
    let temporary = path.with_extension("uniterm-tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(temporary, path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_path(path: &Path) -> PathBuf {
    path.with_extension("json.sha256")
}

fn command_contains_executable(command: &str, executable: &str) -> bool {
    let mut words = command.split(|ch: char| ch.is_whitespace() || ch == '\0');
    let Some(program) = words.next() else {
        return false;
    };
    let basename = program.rsplit('/').next().unwrap_or(program);
    let matches = |name: &str| name == executable || name.starts_with(&format!("{executable}-"));
    if matches(basename) {
        return true;
    }
    // Only an interpreter's script position may identify a packaged CLI.
    // Project directories, prompts and arbitrary later arguments are not
    // executable evidence (e.g. `grok /work/claude-code`).
    if matches!(basename, "sh" | "bash" | "dash" | "zsh" | "fish") {
        return words
            .next()
            .filter(|word| !word.starts_with('-'))
            .is_some_and(|script| matches(script.rsplit('/').next().unwrap_or(script)));
    }
    if matches!(
        basename,
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3"
    ) {
        if let Some(script) = words.next().filter(|word| !word.starts_with('-')) {
            return script.split('/').any(matches);
        }
    }
    false
}

/// Resolve a launch candidate using its real executable target before its
/// ambiguous PATH alias. This is I/O on the launch/runtime side only.
pub(crate) fn launch_identity(command: &str) -> Option<String> {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    let catalog = CATALOG.get_or_init(|| Catalog::load_from_paths(None, None, None));
    std::fs::canonicalize(command)
        .ok()
        .and_then(|path| catalog.process(&path.to_string_lossy()))
        .or_else(|| catalog.process(command))
        .and_then(|found| found.agent)
}

fn safe_evidence(value: &str, limit: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len().saturating_add(character.len_utf8()) > limit {
            break;
        }
        output.push(character);
    }
    output
}

fn expand_home(path: &str) -> Option<PathBuf> {
    if path == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Some(PathBuf::from(std::env::var_os("HOME")?).join(rest));
    }
    Some(PathBuf::from(path))
}

fn read_file_tail(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(limit)))?;
    let mut output = Vec::with_capacity(length.min(limit) as usize);
    file.read_to_end(&mut output)?;
    Ok(output)
}

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("uniterm/providers.json"));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".config/uniterm/providers.json"))
}

fn cache_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("uniterm/providers.json"));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache/uniterm/providers.json"))
}

fn last_good_path() -> PathBuf {
    crate::persist::state_dir().join("providers.last-good.json")
}

/// Exact local and cache files whose parent directories are watched. Watching
/// directories, not file inodes, keeps atomic replacement observable.
pub fn watched_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = config_path() {
        paths.push(path);
    }
    if let Some(path) = cache_path() {
        paths.push(digest_path(&path));
        paths.push(path);
    }
    paths
}

/// Owns the notify registrations that trigger bounded, event-driven reloads.
pub struct ManifestWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl ManifestWatcher {
    pub fn start(changed: crossbeam_channel::Sender<()>) -> Result<Self, String> {
        use notify::Watcher as _;
        let mut paths = Vec::new();
        for path in watched_paths() {
            let Some(parent) = path.parent() else {
                continue;
            };
            crate::persist::ensure_private_dir(parent).map_err(|error| error.to_string())?;
            let parent = std::fs::canonicalize(parent).map_err(|error| error.to_string())?;
            paths.push(parent.join(path.file_name().unwrap_or_default()));
        }
        let wanted = paths.iter().cloned().collect::<HashSet<_>>();
        let mut watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                let Ok(event) = event else {
                    return;
                };
                if event.paths.iter().any(|path| wanted.contains(path)) {
                    let _ = changed.try_send(());
                }
            })
            .map_err(|error| error.to_string())?;
        let mut parents = HashSet::new();
        for path in paths {
            let Some(parent) = path.parent() else {
                continue;
            };
            crate::persist::ensure_private_dir(parent).map_err(|error| error.to_string())?;
            if parents.insert(parent.to_path_buf()) {
                watcher
                    .watch(parent, notify::RecursiveMode::NonRecursive)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
        }
        Ok(Self { _watcher: watcher })
    }
}

/// Idle rests on a positive or default screen verdict rather than on silence,
/// so its dwell only has to outlast the redraw flicker between a spinner
/// frame and the next, not a whole quiet period.
pub const IDLE_DWELL_MS: u64 = 600;

fn default_dwell_ms(status: AgentStatus) -> u64 {
    match status {
        AgentStatus::Permission | AgentStatus::Question => 5_000,
        AgentStatus::Idle => IDLE_DWELL_MS,
        AgentStatus::Error | AgentStatus::Exited => 2_000,
        AgentStatus::Unknown | AgentStatus::Starting | AgentStatus::Working | AgentStatus::Tool => {
            0
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Verify a persisted native-resume argv belongs to a bundled provider. Cache
/// and local manifests are detection data, not authority to execute commands
/// during restore.
pub fn resume_argv_allowed(provider: &str, argv0: &str) -> bool {
    if argv0.is_empty() || argv0.contains('/') {
        return false;
    }
    BUILT_INS
        .iter()
        .find(|manifest| manifest.id == provider)
        .is_some_and(|manifest| manifest.executables.contains(&argv0))
}

/// Provider-owned global instruction/configuration locations, without repo files.
pub(crate) fn global_files() -> Vec<(String, std::path::PathBuf)> {
    claude::global_files()
        .into_iter()
        .chain(codex::global_files())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled_catalog() -> Catalog {
        Catalog {
            manifests: bundled(),
            diagnostics: Vec::new(),
            activation_valid: true,
        }
    }

    fn document(id: &str, alias: &str, pattern: &str) -> ManifestDocument {
        ManifestDocument {
            schema_version: 1,
            manifest_version: "2026.08.1".into(),
            providers: vec![ProviderDefinition {
                id: id.into(),
                executable_aliases: vec![alias.into()],
                capabilities: vec![ManifestCapability::Process, ManifestCapability::Screen],
                log_path: None,
                rules: vec![ManifestRule {
                    id: "screen.waiting".into(),
                    evidence: ManifestEvidence::Screen,
                    status: ManifestStatus::Permission,
                    pattern: pattern.into(),
                    confidence: 93,
                    dwell_ms: Some(1234),
                    anchor: RuleAnchor::Anywhere,
                    region: RuleRegion::Bottom,
                    priority: DEFAULT_RULE_PRIORITY,
                }],
            }],
        }
    }

    #[test]
    fn claude_static_titles_and_completed_summaries_are_idle() {
        let catalog = bundled_catalog();
        for (tail, title) in [
            ("", "✳ Fix connector"),
            ("✻ Worked for 2m 10s\n❯ ", "✳ Fix connector"),
            ("❯ explain running… and esc to interrupt", ""),
            ("✻Thinking…", ""),
        ] {
            assert_eq!(
                catalog.screen("claude", tail, title).unwrap().status,
                Some(AgentStatus::Idle),
                "{tail:?} {title:?}"
            );
        }
        for (tail, title) in [
            ("", "◐ Fix connector"),
            ("", "⠋ Fix connector"),
            ("✻ Thinking… (2s)", "✳ Fix connector"),
            ("· Thinking…", ""),
        ] {
            assert_eq!(
                catalog.screen("claude", tail, title).unwrap().status,
                Some(AgentStatus::Working)
            );
        }
    }

    #[test]
    fn every_provider_live_blocker_beats_a_busy_title() {
        let catalog = bundled_catalog();
        for (id, prompt) in [
            ("claude", "Do you want to proceed?"),
            ("codex", "Would you like to run the following command?"),
            ("opencode", "Permission required"),
            ("gemini", "Allow execution?"),
            ("grok", "Approval required"),
            ("kiro", "Allow this action"),
            ("cursor", "Allow this command"),
            ("pi", "Project trust"),
        ] {
            assert_eq!(
                catalog.screen(id, prompt, "⠋ Working").unwrap().status,
                Some(AgentStatus::Permission),
                "{id}"
            );
        }
    }

    #[test]
    fn process_matching_uses_executable_boundaries() {
        let catalog = bundled_catalog();
        assert_eq!(
            catalog
                .process("/usr/bin/claude --resume")
                .unwrap()
                .agent
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            catalog.process("/usr/bin/claude --resume").unwrap().status,
            None
        );
        assert!(catalog.process("echo claude").is_none());
        assert!(catalog.process("claudette").is_none());
        assert_eq!(
            catalog
                .process("node /opt/@anthropic-ai/claude-code/cli.js")
                .unwrap()
                .agent
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            catalog
                .process("/home/dev/.local/bin/cursor-agent --resume")
                .unwrap()
                .agent
                .as_deref(),
            Some("cursor")
        );
    }

    #[test]
    fn process_identity_ignores_project_paths_and_prompt_arguments() {
        let catalog = bundled_catalog();
        for command in [
            "grok /work/claude-code/fix.md",
            "/work/claude-code/grok-1.0.50-linux-x86_64 --resume",
            "grok --prompt /opt/codex/task",
        ] {
            assert_eq!(
                catalog.process(command).unwrap().agent.as_deref(),
                Some("grok")
            );
        }
        for command in [
            "cat /tmp/claude-code/log",
            "sh /tmp/claude/task",
            "node app.js /opt/claude-code/cli.js",
        ] {
            assert!(catalog.process(command).is_none(), "{command}");
        }
    }

    #[test]
    fn grok_live_status_line_and_blocked_dialogs() {
        let catalog = bundled_catalog();
        for text in [
            "⠧ Waiting for response… 4.3s     1m24s ⇣61.8k [stop]",
            "⠴ Explore /tmp/… + 1 more… 5.6s [stop]",
        ] {
            assert_eq!(
                catalog.screen("grok", text, "").unwrap().status,
                Some(AgentStatus::Working)
            );
        }
        assert_eq!(
            catalog
                .screen("grok", "Ctrl+o:yolo │ Ctrl+c:cancel", "⠧ working")
                .unwrap()
                .status,
            Some(AgentStatus::Permission)
        );
        assert_eq!(
            catalog
                .screen("grok", "Tab:scrollback │ Shift+x:dismiss", "⠧ working")
                .unwrap()
                .status,
            Some(AgentStatus::Question)
        );
        for text in [
            "⠧ logo",
            "› explain [stop]",
            "Build anything",
            "Background tasks: 2 running",
        ] {
            assert_eq!(
                catalog.screen("grok", text, "grok").unwrap().status,
                Some(AgentStatus::Idle)
            );
        }
    }

    #[test]
    fn every_registry_provider_has_builtin_detection_rules() {
        for provider in uniterm_core::agent::PROVIDERS {
            assert!(
                BUILT_INS.iter().any(|manifest| manifest.id == provider.id),
                "{} is missing a detection manifest",
                provider.id
            );
        }
    }

    #[test]
    fn empty_and_plain_titles_never_imply_working_for_any_provider() {
        let catalog = bundled_catalog();
        for provider in uniterm_core::agent::PROVIDERS {
            for title in ["", " ", "uniterm-cli", provider.name] {
                let verdict = catalog.screen(provider.id, "", title).unwrap();
                assert_eq!(
                    verdict.status,
                    Some(AgentStatus::Idle),
                    "{} with title {title:?}: {}",
                    provider.id,
                    verdict.evidence
                );
            }
        }
    }

    #[test]
    fn typed_prompt_text_never_matches_an_activity_rule() {
        let catalog = bundled_catalog();
        // The user is composing a prompt in codex's input box. The words
        // "thinking" and "running" are in the bottom rows, but not on an
        // activity line, so the screen verdict is the idle default.
        let typing =
            "› why is it running slow, keep thinking about the parser\n  ask codex anything";
        let verdict = catalog.screen("codex", typing, "").unwrap();
        assert_eq!(verdict.status, Some(AgentStatus::Idle));
        // Plain keyboard echo with nothing recognisable is idle too, not
        // Working: output volume is never evidence.
        let echo = catalog.screen("claude", "> hello there", "").unwrap();
        assert_eq!(echo.status, Some(AgentStatus::Idle));
        assert_eq!(
            echo.provenance.matched_rule.as_deref(),
            Some("screen.idle.default")
        );
        assert_eq!(echo.provenance.dwell_ms, Some(IDLE_DWELL_MS));
    }

    #[test]
    fn activity_needs_a_spinner_or_anchored_line_and_titles_win() {
        let catalog = bundled_catalog();
        let spinner = catalog
            .screen(
                "codex",
                "  \u{280B} Working (12s • esc to interrupt)\n› ",
                "",
            )
            .unwrap();
        assert_eq!(spinner.status, Some(AgentStatus::Working));
        let stars = catalog
            .screen("claude", "\u{273B} Thinking\u{2026} (esc to interrupt)", "")
            .unwrap();
        assert_eq!(stars.status, Some(AgentStatus::Working));
        // A busy spinner in the window title outranks idle text on the grid.
        let title = catalog
            .screen("codex", "ask codex anything", "\u{2819} codex")
            .unwrap();
        assert_eq!(title.status, Some(AgentStatus::Working));
        assert!(title.evidence.starts_with("window title"));
        // An "Action Required" title outranks even the spinner.
        let blocked = catalog
            .screen("codex", "\u{280B} Working", "Action Required: codex")
            .unwrap();
        assert_eq!(blocked.status, Some(AgentStatus::Permission));
        // Line-start anchoring: the same words indented behind a prompt
        // marker are not an activity line.
        let anchored = catalog.screen("pi", "working... on it", "").unwrap();
        assert_eq!(anchored.status, Some(AgentStatus::Working));
        let typed = catalog.screen("pi", "> working... on it", "").unwrap();
        assert_eq!(typed.status, Some(AgentStatus::Idle));
        assert!(catalog.screen("unknown-agent", "anything", "").is_none());
        assert!(is_spinner_glyph('\u{2800}') && is_spinner_glyph('\u{25D0}'));
        assert!(!is_spinner_glyph('>') && !is_spinner_glyph('\u{00B7}'));
    }

    #[test]
    fn manifest_rules_default_to_anywhere_bottom_and_median_priority() {
        let json = r#"{"id":"screen.custom","evidence":"screen","status":"working","pattern":"synthesizing","confidence":80}"#;
        let rule: ManifestRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.anchor, RuleAnchor::Anywhere);
        assert_eq!(rule.region, RuleRegion::Bottom);
        assert_eq!(rule.priority, DEFAULT_RULE_PRIORITY);
        let json = r#"{"id":"screen.spin","evidence":"screen","status":"working","pattern":"","confidence":80,"anchor":"spinner_line","region":"title","priority":110}"#;
        let rule: ManifestRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.anchor, RuleAnchor::SpinnerLine);
        assert_eq!(rule.region, RuleRegion::Title);
        assert_eq!(rule.priority, 110);
    }

    #[test]
    fn permission_rule_outranks_idle_rule() {
        let result = bundled_catalog()
            .screen("claude", "Do you want to proceed?  Yes / No", "")
            .unwrap();
        assert_eq!(result.status, Some(AgentStatus::Permission));
        assert_eq!(result.provenance.source, DetectionSource::Bundled);
        assert!(result.provenance.matched_rule.is_some());
    }

    #[test]
    fn versioned_rules_preserve_explanation_hints() {
        let mut selected = HashMap::new();
        insert_document(
            &mut selected,
            document("custom", "custom-agent", "reviewer approval"),
            DetectionSource::LocalOverride,
            LOCAL_PRECEDENCE,
        );
        let catalog = Catalog {
            manifests: selected.into_values().collect(),
            diagnostics: Vec::new(),
            activation_valid: true,
        };
        let found = catalog
            .screen("custom", "Waiting for reviewer approval", "")
            .unwrap();
        assert_eq!(found.status, Some(AgentStatus::Permission));
        assert_eq!(
            found.provenance.manifest_version.as_deref(),
            Some("2026.08.1")
        );
        assert_eq!(
            found.provenance.matched_rule.as_deref(),
            Some("screen.waiting")
        );
        assert_eq!(found.provenance.confidence, Some(93));
        assert_eq!(found.provenance.dwell_ms, Some(1234));
        assert_eq!(found.provenance.precedence, LOCAL_PRECEDENCE);
    }

    #[test]
    fn validation_rejects_broad_control_and_unbounded_data() {
        let mut valid = document("custom", "custom-agent", "reviewer approval");
        assert!(validate_document(&valid).is_ok());
        valid.providers[0].rules[0].pattern = "yes".into();
        assert!(validate_document(&valid).unwrap_err().contains("overbroad"));
        valid.providers[0].rules[0].pattern = "approval\u{1b}[31m".into();
        assert!(validate_document(&valid).unwrap_err().contains("control"));
        valid.providers[0].rules[0].pattern = "x".repeat(MAX_PATTERN_BYTES + 1);
        assert!(validate_document(&valid).unwrap_err().contains("at most"));

        let mut alias = document("custom", "sh", "reviewer approval");
        assert!(validate_document(&alias)
            .unwrap_err()
            .contains("overbroad interpreter"));
        alias.providers[0].executable_aliases = vec!["custom-agent".into()];
        alias.providers[0]
            .capabilities
            .push(ManifestCapability::Connector);
        assert!(validate_document(&alias)
            .unwrap_err()
            .contains("first-party support"));

        let mut collision = document("first", "shared-agent", "reviewer approval");
        let mut second = collision.providers[0].clone();
        second.id = "second".into();
        collision.providers.push(second);
        assert!(validate_document(&collision)
            .unwrap_err()
            .contains("claimed by both"));
    }

    #[test]
    fn verified_digest_and_last_good_are_exact() {
        let root =
            std::env::temp_dir().join(format!("uniterm-provider-digest-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("providers.json");
        let bytes =
            serde_json::to_vec(&document("custom", "custom-agent", "reviewer approval")).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        std::fs::write(digest_path(&path), format!("{}\n", sha256_hex(&bytes))).unwrap();
        assert!(read_verified(&path).unwrap().is_some());
        std::fs::write(&path, b"{}").unwrap();
        assert!(read_verified(&path)
            .unwrap_err()
            .contains("digest does not match"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn local_cache_last_good_and_bundled_precedence_is_explicit() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-provider-precedence-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let local = root.join("local.json");
        let cache = root.join("cache.json");
        let last_good = root.join("last-good.json");

        let mut cached = document("cached-agent", "codex", "cached permission");
        cached.manifest_version = "cache-v1".into();
        let cache_bytes = serde_json::to_vec(&cached).unwrap();
        std::fs::write(&cache, &cache_bytes).unwrap();
        std::fs::write(
            digest_path(&cache),
            format!("{}\n", sha256_hex(&cache_bytes)),
        )
        .unwrap();

        let mut override_document = document("local-agent", "codex", "local permission");
        override_document.manifest_version = "local-v1".into();
        std::fs::write(&local, serde_json::to_vec(&override_document).unwrap()).unwrap();

        let catalog = Catalog::load_from_paths(Some(&local), Some(&cache), Some(&last_good));
        let found = catalog.process("/usr/bin/codex").unwrap();
        assert_eq!(found.agent.as_deref(), Some("local-agent"));
        assert_eq!(found.provenance.source, DetectionSource::LocalOverride);
        assert_eq!(found.provenance.precedence, LOCAL_PRECEDENCE);

        std::fs::remove_file(&local).unwrap();
        let catalog = Catalog::load_from_paths(None, Some(&cache), Some(&last_good));
        let found = catalog.process("/usr/bin/codex").unwrap();
        assert_eq!(found.agent.as_deref(), Some("cached-agent"));
        assert_eq!(found.provenance.source, DetectionSource::VerifiedCache);

        std::fs::write(&cache, b"corrupt cache").unwrap();
        let catalog = Catalog::load_from_paths(None, Some(&cache), Some(&last_good));
        let found = catalog.process("/usr/bin/codex").unwrap();
        assert_eq!(found.agent.as_deref(), Some("cached-agent"));
        assert_eq!(found.provenance.source, DetectionSource::LastKnownGood);
        assert_eq!(found.provenance.precedence, LAST_GOOD_PRECEDENCE);

        std::fs::write(&local, b"not valid JSON").unwrap();
        let rejected = Catalog::load_from_paths(Some(&local), Some(&cache), Some(&last_good));
        assert!(!rejected.activation_valid());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_argv_is_checked_against_the_trusted_builtin_boundary() {
        assert!(resume_argv_allowed("codex", "codex"));
        assert!(resume_argv_allowed("cursor", "cursor-agent"));
        assert!(!resume_argv_allowed("my-custom-agent", "my-custom-agent"));
        assert!(!resume_argv_allowed("codex", "/tmp/codex"));
        assert!(!resume_argv_allowed("codex", "codex-malicious"));
    }

    #[test]
    fn a_reported_session_id_gives_the_providers_own_resume_command() {
        assert_eq!(
            resume_argv("opencode", "ses_e70de45c9ffe").unwrap(),
            ["opencode", "--session", "ses_e70de45c9ffe"]
        );
        assert_eq!(
            resume_argv("codex", "abc-1").unwrap(),
            ["codex", "resume", "abc-1"]
        );
        for manifest in BUILT_INS {
            let argv = resume_argv(manifest.id, "id")
                .unwrap_or_else(|| panic!("{} has no resume flag", manifest.id));
            assert!(
                resume_argv_allowed(manifest.id, &argv[0]),
                "{}",
                manifest.id
            );
            assert_eq!(argv.last().unwrap(), "id");
        }
        assert!(resume_argv("my-custom-agent", "id").is_none());
        assert!(resume_argv("claude", "--dangerously-skip-permissions").is_none());
        assert!(resume_argv("claude", "a b").is_none());
        assert!(resume_argv("claude", "").is_none());
    }

    #[test]
    fn completing_a_profile_fills_only_an_empty_resume_command() {
        let mut empty = crate::persist::AgentLaunchSnap {
            provider: "claude".into(),
            session_id: Some("s1".into()),
            resume_command: Vec::new(),
        };
        complete_resume(&mut empty);
        assert_eq!(empty.resume_command, ["claude", "--resume", "s1"]);
        let mut recorded = crate::persist::AgentLaunchSnap {
            resume_command: vec!["claude".into(), "--continue".into()],
            ..empty.clone()
        };
        complete_resume(&mut recorded);
        assert_eq!(recorded.resume_command, ["claude", "--continue"]);
        let mut unknown = crate::persist::AgentLaunchSnap {
            session_id: None,
            resume_command: Vec::new(),
            ..empty
        };
        complete_resume(&mut unknown);
        assert!(unknown.resume_command.is_empty());
    }
}

#[cfg(test)]
mod task_launch_tests {
    use super::*;

    #[test]
    fn every_selectable_provider_has_an_interactive_launch_contract() {
        for provider in uniterm_core::agent::PROVIDERS {
            let args = launch_argv(
                provider.id,
                "/tmp/agent executable",
                "literal ' $(text)\n--help",
                None,
                None,
            )
            .unwrap();
            assert_eq!(args.first().unwrap(), "/tmp/agent executable");
            assert!(args.last().unwrap().ends_with("literal ' $(text)\n--help"));
            assert!(!args.iter().any(|arg| matches!(
                arg.as_str(),
                "--print" | "--no-interactive" | "--yolo" | "--always-approve"
            )));
        }
        assert_eq!(
            launch_argv("opencode", "opencode", "hello", None, None).unwrap(),
            ["opencode", "--prompt=hello"]
        );
        assert_eq!(
            launch_argv("gemini", "gemini", "hello", None, None).unwrap(),
            ["gemini", "--prompt-interactive=hello"]
        );
        assert_eq!(
            launch_argv("kiro", "kiro-cli", "hello", None, None).unwrap(),
            ["kiro-cli", "chat", "--", "hello"]
        );
        for provider in ["claude", "codex", "grok", "cursor", "pi"] {
            assert_eq!(
                launch_argv(provider, provider, "--help", None, None).unwrap(),
                [provider, "--", "--help"]
            );
        }
    }

    #[test]
    fn unsupported_options_are_rejected_and_custom_argv_stays_compatible() {
        assert!(launch_argv("custom", "custom", "prompt", Some("model"), None).is_err());
        assert!(launch_argv("claude", "claude", "prompt", None, Some("profile")).is_err());
        assert_eq!(
            launch_argv("custom", "/custom", "prompt", None, None).unwrap(),
            ["/custom", "prompt"]
        );
        assert_eq!(
            launch_argv("codex", "codex", "prompt", Some("model"), Some("profile")).unwrap(),
            [
                "codex",
                "--model=model",
                "--profile=profile",
                "--",
                "prompt"
            ]
        );
    }
}
