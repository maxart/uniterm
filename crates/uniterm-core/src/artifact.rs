//! Pure typed artifact ownership and lifecycle projection.
//!
//! Filesystem validation, hashing, and watching live in `uniterm-server`.
//! This module accepts only bounded facts so recovery, control reads, and UI
//! projections share one deterministic model without I/O or async work.

use std::collections::{BTreeMap, HashMap};

use crate::{ProjectId, RoleId, RunId};

/// Maximum retained artifact records in the live projection.
pub const ARTIFACT_LEDGER_CAP: usize = 4_096;
/// Maximum UTF-8 byte length of a Project-relative artifact path.
pub const ARTIFACT_PATH_MAX_BYTES: usize = 4_096;

/// Workspace-local monotonic artifact identity.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ArtifactId(
    /// Monotonic Workspace-local value, with zero reserved as invalid.
    pub u64,
);

/// Stable artifact classes understood without knowing a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A produced file without a more specific semantic class.
    File,
    /// A plan intended to guide later roles or humans.
    Plan,
    /// A patch or diff that can be reviewed or applied.
    Patch,
    /// A human-readable outcome or implementation report.
    Report,
    /// Test output or another bounded verification record.
    TestEvidence,
    /// Verifier findings that are not yet review annotations.
    Findings,
}

impl ArtifactKind {
    /// Stable human-facing spelling shared by CLI and inspection surfaces.
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Plan => "plan",
            Self::Patch => "patch",
            Self::Report => "report",
            Self::TestEvidence => "test-evidence",
            Self::Findings => "findings",
        }
    }
}

/// Current availability of one retained artifact observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// The last event-driven observation found a non-empty regular file.
    Available,
    /// The current path no longer resolves to an acceptable file.
    Missing,
    /// A later producer observation owns the same Project path.
    Superseded,
}

impl ArtifactStatus {
    /// Stable human-facing spelling shared by CLI and inspection surfaces.
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Missing => "missing",
            Self::Superseded => "superseded",
        }
    }
}

/// One immutable producer observation plus its latest filesystem facts.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRecord {
    /// Stable identity used by lifecycle events and later references.
    pub id: ArtifactId,
    /// Canonical Project owner, never inferred from a Pane during reads.
    pub project: ProjectId,
    /// Run that owned the active completion token at observation time.
    pub producer_run: RunId,
    /// Role that owned the active completion token at observation time.
    pub producer_role: RoleId,
    /// Provider-neutral semantic class supplied by the completion contract.
    pub kind: ArtifactKind,
    /// Canonical Project-relative path using `/` separators.
    pub path: String,
    /// Lowercase SHA-256 digest of the observed file contents.
    pub digest: String,
    /// Bytes actually read while computing `digest`.
    pub size: u64,
    /// Latest lifecycle state for this retained identity.
    pub status: ArtifactStatus,
    /// Artifact at this path that this observation replaced, if any.
    pub supersedes: Option<ArtifactId>,
}

/// Append-only lifecycle facts. A new observation at an existing path
/// supersedes its prior current record atomically in the reducer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArtifactEvent {
    /// Create one producer-owned identity from runtime-authoritative facts.
    Observed {
        artifact: ArtifactId,
        project: ProjectId,
        producer_run: RunId,
        producer_role: RoleId,
        kind: ArtifactKind,
        path: String,
        digest: String,
        size: u64,
    },
    /// Replace filesystem facts without changing producer ownership.
    Refreshed {
        artifact: ArtifactId,
        digest: String,
        size: u64,
    },
    /// Retain ownership after the current file disappears.
    Missing { artifact: ArtifactId },
}

/// Invalid durable artifact data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactError(
    /// Bounded projection failure suitable for durability diagnostics.
    pub String,
);

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArtifactError {}

/// Bounded current artifact projection with direct ownership and path indexes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactLedger {
    records: BTreeMap<ArtifactId, ArtifactRecord>,
    by_project: HashMap<ProjectId, Vec<ArtifactId>>,
    by_run: HashMap<RunId, Vec<ArtifactId>>,
    by_role: HashMap<RoleId, Vec<ArtifactId>>,
    current_path: HashMap<(ProjectId, String), ArtifactId>,
    next_artifact: u64,
}

impl Default for ArtifactLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtifactLedger {
    /// Start an empty Workspace-local projection at identity one.
    pub fn new() -> Self {
        Self {
            records: BTreeMap::new(),
            by_project: HashMap::new(),
            by_run: HashMap::new(),
            by_role: HashMap::new(),
            current_path: HashMap::new(),
            next_artifact: 1,
        }
    }

    /// Reserve the next identity only in a staged event, not as mutable state.
    pub fn next_artifact_id(&self) -> ArtifactId {
        ArtifactId(self.next_artifact)
    }

    /// Resolve one Artifact directly by its stable identity.
    pub fn artifact(&self, id: ArtifactId) -> Option<&ArtifactRecord> {
        self.records.get(&id)
    }

    /// Iterate retained records in stable identity order for complete reads.
    pub fn artifacts(&self) -> impl Iterator<Item = &ArtifactRecord> {
        self.records.values()
    }

    /// Resolve retained Artifact identities for one Project without a scan.
    pub fn for_project(&self, project: ProjectId) -> &[ArtifactId] {
        self.by_project.get(&project).map_or(&[], Vec::as_slice)
    }

    /// Resolve retained Artifact identities for one producer Run without a scan.
    pub fn for_run(&self, run: RunId) -> &[ArtifactId] {
        self.by_run.get(&run).map_or(&[], Vec::as_slice)
    }

    /// Resolve retained Artifact identities for one producer Role without a scan.
    pub fn for_role(&self, role: RoleId) -> &[ArtifactId] {
        self.by_role.get(&role).map_or(&[], Vec::as_slice)
    }

    /// Resolve the current identity at one canonical Project-relative path.
    pub fn current_at(&self, project: ProjectId, path: &str) -> Option<ArtifactId> {
        self.current_path.get(&(project, path.to_string())).copied()
    }

    /// Apply one append-only fact atomically across every ownership index.
    pub fn apply(&mut self, event: ArtifactEvent) -> Result<(), ArtifactError> {
        match event {
            ArtifactEvent::Observed {
                artifact,
                project,
                producer_run,
                producer_role,
                kind,
                path,
                digest,
                size,
            } => {
                validate_observation(
                    artifact,
                    project,
                    producer_run,
                    producer_role,
                    &path,
                    &digest,
                    size,
                )?;
                if self.records.contains_key(&artifact) {
                    return Err(ArtifactError(format!("duplicate Artifact {}", artifact.0)));
                }
                if artifact.0 < self.next_artifact {
                    return Err(ArtifactError(format!(
                        "Artifact {} is older than the next identity {}",
                        artifact.0, self.next_artifact
                    )));
                }
                let key = (project, path.clone());
                let supersedes = self.current_path.get(&key).copied();
                self.make_room(supersedes)?;
                if let Some(previous) = supersedes {
                    if let Some(previous) = self.records.get_mut(&previous) {
                        previous.status = ArtifactStatus::Superseded;
                    }
                }
                self.current_path.insert(key, artifact);
                self.records.insert(
                    artifact,
                    ArtifactRecord {
                        id: artifact,
                        project,
                        producer_run,
                        producer_role,
                        kind,
                        path,
                        digest,
                        size,
                        status: ArtifactStatus::Available,
                        supersedes,
                    },
                );
                self.by_project.entry(project).or_default().push(artifact);
                self.by_run.entry(producer_run).or_default().push(artifact);
                self.by_role
                    .entry(producer_role)
                    .or_default()
                    .push(artifact);
                self.next_artifact = artifact.0.saturating_add(1);
            }
            ArtifactEvent::Refreshed {
                artifact,
                digest,
                size,
            } => {
                validate_digest(&digest)?;
                if size == 0 {
                    return Err(ArtifactError(
                        "available Artifact size must be nonzero".into(),
                    ));
                }
                let record = self
                    .records
                    .get_mut(&artifact)
                    .ok_or_else(|| ArtifactError(format!("unknown Artifact {}", artifact.0)))?;
                if record.status == ArtifactStatus::Superseded {
                    return Err(ArtifactError("cannot refresh a superseded Artifact".into()));
                }
                record.digest = digest;
                record.size = size;
                record.status = ArtifactStatus::Available;
            }
            ArtifactEvent::Missing { artifact } => {
                let record = self
                    .records
                    .get_mut(&artifact)
                    .ok_or_else(|| ArtifactError(format!("unknown Artifact {}", artifact.0)))?;
                if record.status == ArtifactStatus::Superseded {
                    return Err(ArtifactError(
                        "cannot mark a superseded Artifact missing".into(),
                    ));
                }
                record.status = ArtifactStatus::Missing;
            }
        }
        Ok(())
    }

    fn make_room(&mut self, replacement: Option<ArtifactId>) -> Result<(), ArtifactError> {
        if self.records.len() < ARTIFACT_LEDGER_CAP {
            return Ok(());
        }
        let removable = self
            .records
            .iter()
            .find_map(|(id, record)| (record.status != ArtifactStatus::Available).then_some(*id))
            .or(replacement);
        let Some(id) = removable else {
            return Err(ArtifactError(format!(
                "Artifact ledger reached its {} available-record cap",
                ARTIFACT_LEDGER_CAP
            )));
        };
        self.remove_from_projection(id);
        Ok(())
    }

    fn remove_from_projection(&mut self, id: ArtifactId) {
        let Some(record) = self.records.remove(&id) else {
            return;
        };
        remove_id(self.by_project.get_mut(&record.project), id);
        remove_id(self.by_run.get_mut(&record.producer_run), id);
        remove_id(self.by_role.get_mut(&record.producer_role), id);
        let key = (record.project, record.path);
        if self.current_path.get(&key) == Some(&id) {
            self.current_path.remove(&key);
        }
    }
}

fn remove_id(ids: Option<&mut Vec<ArtifactId>>, id: ArtifactId) {
    if let Some(ids) = ids {
        ids.retain(|candidate| *candidate != id);
    }
}

fn validate_observation(
    artifact: ArtifactId,
    project: ProjectId,
    run: RunId,
    role: RoleId,
    path: &str,
    digest: &str,
    size: u64,
) -> Result<(), ArtifactError> {
    if artifact.0 == 0 || artifact.0 == u64::MAX || project.0 == 0 || run.0 == 0 || role.0 == 0 {
        return Err(ArtifactError(
            "Artifact ownership identities must be nonzero".into(),
        ));
    }
    if path.is_empty() || path.len() > ARTIFACT_PATH_MAX_BYTES {
        return Err(ArtifactError(format!(
            "Artifact path must contain between 1 and {ARTIFACT_PATH_MAX_BYTES} bytes"
        )));
    }
    if path.starts_with('/')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || path.chars().any(char::is_control)
    {
        return Err(ArtifactError(
            "Artifact path must be a normalized Project-relative path".into(),
        ));
    }
    validate_digest(digest)?;
    if size == 0 {
        return Err(ArtifactError(
            "available Artifact size must be nonzero".into(),
        ));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), ArtifactError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ArtifactError(
            "Artifact digest must be lowercase SHA-256 hex".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(id: u64, role: u64, path: &str, byte: char) -> ArtifactEvent {
        ArtifactEvent::Observed {
            artifact: ArtifactId(id),
            project: ProjectId(1),
            producer_run: RunId(2),
            producer_role: RoleId(role),
            kind: ArtifactKind::File,
            path: path.into(),
            digest: byte.to_string().repeat(64),
            size: 10,
        }
    }

    #[test]
    fn indexes_ownership_and_supersedes_one_current_path() {
        let mut ledger = ArtifactLedger::new();
        ledger.apply(observed(1, 3, "reports/a.md", 'a')).unwrap();
        ledger.apply(observed(2, 4, "reports/a.md", 'b')).unwrap();
        assert_eq!(
            ledger.current_at(ProjectId(1), "reports/a.md"),
            Some(ArtifactId(2))
        );
        assert_eq!(
            ledger.artifact(ArtifactId(1)).unwrap().status,
            ArtifactStatus::Superseded
        );
        assert_eq!(
            ledger.artifact(ArtifactId(2)).unwrap().supersedes,
            Some(ArtifactId(1))
        );
        assert_eq!(
            ledger.for_project(ProjectId(1)),
            [ArtifactId(1), ArtifactId(2)]
        );
        assert_eq!(ledger.for_run(RunId(2)), [ArtifactId(1), ArtifactId(2)]);
        assert_eq!(ledger.for_role(RoleId(4)), [ArtifactId(2)]);
    }

    #[test]
    fn refresh_and_missing_preserve_identity_and_reject_stale_updates() {
        let mut ledger = ArtifactLedger::new();
        ledger.apply(observed(1, 3, "plan.md", 'a')).unwrap();
        ledger
            .apply(ArtifactEvent::Missing {
                artifact: ArtifactId(1),
            })
            .unwrap();
        assert_eq!(
            ledger.artifact(ArtifactId(1)).unwrap().status,
            ArtifactStatus::Missing
        );
        ledger
            .apply(ArtifactEvent::Refreshed {
                artifact: ArtifactId(1),
                digest: "b".repeat(64),
                size: 11,
            })
            .unwrap();
        assert_eq!(
            ledger.artifact(ArtifactId(1)).unwrap().status,
            ArtifactStatus::Available
        );
        ledger.apply(observed(2, 4, "plan.md", 'c')).unwrap();
        assert!(ledger
            .apply(ArtifactEvent::Missing {
                artifact: ArtifactId(1)
            })
            .is_err());
    }

    #[test]
    fn invalid_paths_digests_and_ownership_fail_closed() {
        for path in ["", "/tmp/a", "../a", "a/../b", "a//b", "a\nb"] {
            let mut ledger = ArtifactLedger::new();
            assert!(
                ledger.apply(observed(1, 3, path, 'a')).is_err(),
                "accepted {path:?}"
            );
        }
        let mut ledger = ArtifactLedger::new();
        let mut event = observed(1, 3, "a.md", 'a');
        if let ArtifactEvent::Observed { digest, .. } = &mut event {
            *digest = "A".repeat(64);
        }
        assert!(ledger.apply(event).is_err());
        assert!(ledger.apply(observed(0, 3, "a.md", 'a')).is_err());
    }

    #[test]
    fn replacement_succeeds_when_every_retained_record_is_available() {
        let mut ledger = ArtifactLedger::new();
        for id in 1..=ARTIFACT_LEDGER_CAP as u64 {
            ledger
                .apply(observed(id, 3, &format!("artifact-{id}"), 'a'))
                .unwrap();
        }
        let replacement = ARTIFACT_LEDGER_CAP as u64 + 1;
        assert!(ledger
            .apply(observed(replacement, 4, "one-too-many", 'b'))
            .is_err());
        ledger
            .apply(observed(replacement, 4, "artifact-1", 'b'))
            .unwrap();
        assert_eq!(ledger.artifacts().count(), ARTIFACT_LEDGER_CAP);
        assert!(ledger.artifact(ArtifactId(1)).is_none());
        assert_eq!(
            ledger.current_at(ProjectId(1), "artifact-1"),
            Some(ArtifactId(replacement))
        );
        assert_eq!(
            ledger.artifact(ArtifactId(replacement)).unwrap().supersedes,
            Some(ArtifactId(1))
        );
    }
}
