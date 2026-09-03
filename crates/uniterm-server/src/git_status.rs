//! Repository-root-keyed, event-driven Git change summaries.
//!
//! A repository is watched only while its Project file manager is visible.
//! Notify bursts are coalesced with a trailing debounce, then one Git scan is
//! shared by every subscribed Project. No periodic timer scans repositories.

use std::collections::{HashMap, HashSet};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;
use mio::Waker;
use notify::Watcher as _;
use uniterm_core::ProjectId;
use uniterm_proto::{AgentToCore, GitChangeStats};

const GIT_DEBOUNCE: Duration = Duration::from_millis(200);

/// Owns visible Project subscriptions and one watcher per canonical Git root.
pub(crate) struct GitWatchManager {
    projects: HashMap<ProjectId, PathBuf>,
    repositories: HashMap<PathBuf, RepositoryWatch>,
}

impl GitWatchManager {
    pub(crate) fn new() -> Self {
        Self {
            projects: HashMap::new(),
            repositories: HashMap::new(),
        }
    }

    /// Replace one Project subscription and return its current summary.
    pub(crate) async fn set(
        &mut self,
        project: ProjectId,
        root: Option<String>,
        tx: Sender<AgentToCore>,
        waker: Arc<Waker>,
    ) -> AgentToCore {
        let Some(root) = root else {
            self.remove_project(project);
            return AgentToCore::GitChangeStats {
                project,
                stats: None,
            };
        };
        let discovered = tokio::task::spawn_blocking(move || discover_and_compute(&root))
            .await
            .ok()
            .flatten();

        let Some((repository, computed)) = discovered else {
            // A visible watcher already has a last-known-good projection.
            // Retain it when a refresh subprocess fails instead of turning a
            // transient resource or resume error into a false empty summary.
            if let Some(stats) = self.current_summary(project) {
                return AgentToCore::GitChangeStats {
                    project,
                    stats: Some(stats),
                };
            }
            self.remove_project(project);
            return AgentToCore::GitChangeStats {
                project,
                stats: None,
            };
        };

        if self.projects.get(&project) != Some(&repository) {
            self.remove_project(project);
        }

        let stats = if let Some(watch) = self.repositories.get(&repository) {
            watch.add_project(project);
            watch.replace_summary(computed.clone());
            computed
        } else {
            match RepositoryWatch::new(repository.clone(), project, computed.clone(), tx, waker) {
                Ok(watch) => {
                    self.repositories.insert(repository.clone(), watch);
                    computed
                }
                // A watcher can be unavailable for huge repositories or when
                // the OS watch limit is exhausted. The initial summary stays
                // useful and Project switching must remain non-fatal.
                Err(_) => computed,
            }
        };
        self.projects.insert(project, repository);

        AgentToCore::GitChangeStats {
            project,
            stats: Some(stats),
        }
    }

    fn current_summary(&self, project: ProjectId) -> Option<GitChangeStats> {
        let repository = self.projects.get(&project)?;
        self.repositories
            .get(repository)
            .map(RepositoryWatch::summary)
    }

    fn remove_project(&mut self, project: ProjectId) {
        let Some(repository) = self.projects.remove(&project) else {
            return;
        };
        let empty = self
            .repositories
            .get(&repository)
            .is_some_and(|watch| watch.remove_project(project));
        if empty {
            self.repositories.remove(&repository);
        }
    }
}

struct RepositoryWatch {
    _watcher: notify::RecommendedWatcher,
    projects: Arc<Mutex<HashSet<ProjectId>>>,
    summary: Arc<Mutex<GitChangeStats>>,
    #[cfg(test)]
    workers_started: Arc<AtomicU64>,
}

impl RepositoryWatch {
    fn new(
        repository: PathBuf,
        project: ProjectId,
        initial: GitChangeStats,
        tx: Sender<AgentToCore>,
        waker: Arc<Waker>,
    ) -> notify::Result<Self> {
        let projects = Arc::new(Mutex::new(HashSet::from([project])));
        let summary = Arc::new(Mutex::new(initial));
        let generation = Arc::new(AtomicU64::new(0));
        let worker_active = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let workers_started = Arc::new(AtomicU64::new(0));
        let handle = tokio::runtime::Handle::current();

        let callback_root = repository.clone();
        let callback_projects = Arc::clone(&projects);
        let callback_summary = Arc::clone(&summary);
        let callback_generation = Arc::clone(&generation);
        let callback_worker_active = Arc::clone(&worker_active);
        #[cfg(test)]
        let callback_workers_started = Arc::clone(&workers_started);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                // Git reads many files while computing a summary. Ignoring
                // access-only notifications prevents the scan from
                // invalidating itself on platforms that report open/close.
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                callback_generation.fetch_add(1, Ordering::AcqRel);
                // A large repository can emit thousands of events in one
                // burst. Keep exactly one trailing-debounce worker active
                // instead of allocating one sleeping task per event.
                if callback_worker_active.swap(true, Ordering::AcqRel) {
                    return;
                }
                #[cfg(test)]
                callback_workers_started.fetch_add(1, Ordering::Relaxed);
                let root = callback_root.clone();
                let projects = Arc::clone(&callback_projects);
                let summary = Arc::clone(&callback_summary);
                let generation = Arc::clone(&callback_generation);
                let worker_active = Arc::clone(&callback_worker_active);
                let tx = tx.clone();
                let waker = Arc::clone(&waker);
                handle.spawn(async move {
                    loop {
                        let ticket = generation.load(Ordering::Acquire);
                        tokio::time::sleep(GIT_DEBOUNCE).await;
                        if generation.load(Ordering::Acquire) != ticket {
                            continue;
                        }
                        let compute_root = root.clone();
                        let computed = tokio::task::spawn_blocking(move || {
                            compute_change_stats(&compute_root)
                        })
                        .await
                        .ok()
                        .flatten();
                        if generation.load(Ordering::Acquire) != ticket {
                            continue;
                        }
                        if let Some(computed) = computed {
                            *lock(&summary) = computed.clone();
                            let subscribed: Vec<ProjectId> =
                                lock(&projects).iter().copied().collect();
                            let mut sent = false;
                            for project in subscribed {
                                sent |= tx
                                    .send(AgentToCore::GitChangeStats {
                                        project,
                                        stats: Some(computed.clone()),
                                    })
                                    .is_ok();
                            }
                            if sent {
                                let _ = waker.wake();
                            }
                        }

                        worker_active.store(false, Ordering::Release);
                        if generation.load(Ordering::Acquire) == ticket
                            || worker_active.swap(true, Ordering::AcqRel)
                        {
                            break;
                        }
                    }
                });
            })?;
        watcher.watch(&repository, notify::RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            projects,
            summary,
            #[cfg(test)]
            workers_started,
        })
    }

    fn add_project(&self, project: ProjectId) {
        lock(&self.projects).insert(project);
    }

    /// Remove a subscriber and report whether this repository is now unused.
    fn remove_project(&self, project: ProjectId) -> bool {
        let mut projects = lock(&self.projects);
        projects.remove(&project);
        projects.is_empty()
    }

    fn replace_summary(&self, summary: GitChangeStats) {
        *lock(&self.summary) = summary;
    }

    fn summary(&self) -> GitChangeStats {
        lock(&self.summary).clone()
    }
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn discover_and_compute(project_root: &str) -> Option<(PathBuf, GitChangeStats)> {
    let root = discover_repository(project_root)?;
    let stats = compute_change_stats(&root)?;
    Some((root, stats))
}

fn discover_repository(project_root: &str) -> Option<PathBuf> {
    let output = run_git(Path::new(project_root), &["rev-parse", "--show-toplevel"])?;
    let value = String::from_utf8_lossy(&output);
    let root = PathBuf::from(value.trim());
    std::fs::canonicalize(root).ok()
}

fn compute_change_stats(repository: &Path) -> Option<GitChangeStats> {
    let diff_args = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--shortstat",
        "HEAD",
        "--",
    ];
    let shortstat = run_git(repository, &diff_args).or_else(|| {
        // An unborn repository has no HEAD, but its staged and untracked
        // files are still a valid summary. Confirm Git itself is healthy
        // before using that fallback so a transient command failure does not
        // masquerade as an empty repository.
        run_git(repository, &["rev-parse", "--is-inside-work-tree"])?;
        if run_git(repository, &["rev-parse", "--verify", "HEAD"]).is_some() {
            return None;
        }
        run_git(
            repository,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--shortstat",
                "--cached",
                "--",
            ],
        )
    })?;
    let shortstat = String::from_utf8_lossy(&shortstat);
    let (files_changed, insertions, deletions) = parse_shortstat(&shortstat);
    let untracked = count_untracked(repository)?;
    Some(GitChangeStats {
        files_changed,
        insertions,
        deletions,
        untracked,
    })
}

fn run_git(directory: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Count NUL-delimited untracked paths without buffering a large repository's
/// entire file list in memory.
fn count_untracked(repository: &Path) -> Option<u32> {
    let Ok(mut child) = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(repository)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let mut reader = std::io::BufReader::new(stdout);
    let mut path = Vec::new();
    let mut count = 0_u32;
    loop {
        path.clear();
        match reader.read_until(0, &mut path) {
            Ok(0) => break,
            Ok(_) => count = count.saturating_add(1),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    child
        .wait()
        .ok()
        .filter(|status| status.success())
        .map(|_| count)
}

fn parse_shortstat(value: &str) -> (u32, u32, u32) {
    let (mut files, mut insertions, mut deletions) = (0, 0, 0);
    for segment in value.trim().split(',') {
        let mut words = segment.split_whitespace();
        let Some(count) = words.next().and_then(|word| word.parse::<u32>().ok()) else {
            continue;
        };
        match words.next() {
            Some(word) if word.starts_with("file") => files = count,
            Some(word) if word.starts_with("insertion") => insertions = count,
            Some(word) if word.starts_with("deletion") => deletions = count,
            _ => {}
        }
    }
    (files, insertions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Poll, Token};
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    static NEXT_REPOSITORY: AtomicUsize = AtomicUsize::new(1);

    fn repository() -> PathBuf {
        let unique = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "uniterm-git-status-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "test@uniterm.local"]);
        git(&root, &["config", "user.name", "Uniterm Test"]);
        fs::write(root.join("README.md"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        root
    }

    fn git(root: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root)
            .env("LC_ALL", "C")
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn parses_shortstat_variants() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 45 insertions(+), 6 deletions(-)"),
            (3, 45, 6)
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 7 deletions(-)"),
            (1, 0, 7)
        );
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn counts_tracked_and_untracked_changes() {
        let root = repository();
        fs::write(root.join("README.md"), "one\nchanged\n").unwrap();
        fs::write(root.join("scratch.txt"), "new\n").unwrap();
        let stats = compute_change_stats(&root).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 1);
        assert_eq!(stats.deletions, 2);
        assert_eq!(stats.untracked, 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn failed_git_reads_are_not_reported_as_empty_summaries() {
        let missing =
            std::env::temp_dir().join(format!("uniterm-missing-git-status-{}", std::process::id()));
        assert_eq!(compute_change_stats(&missing), None);
    }

    #[test]
    fn unborn_repositories_still_report_untracked_files() {
        let root = std::env::temp_dir().join(format!(
            "uniterm-unborn-git-status-{}",
            NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        fs::write(root.join("new.txt"), "new\n").unwrap();
        let stats = compute_change_stats(&root).unwrap();
        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.untracked, 1);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_failure_retains_the_last_known_good_summary() {
        let root = repository();
        fs::write(root.join("README.md"), "changed\n").unwrap();
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(90)).unwrap());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut manager = GitWatchManager::new();
        let initial = manager
            .set(
                ProjectId(6),
                Some(root.to_string_lossy().into_owned()),
                tx.clone(),
                Arc::clone(&waker),
            )
            .await;
        let AgentToCore::GitChangeStats {
            stats: Some(expected),
            ..
        } = initial
        else {
            panic!("initial Git summary was unavailable");
        };

        let missing = root.join("missing-project-root");
        let refreshed = manager
            .set(
                ProjectId(6),
                Some(missing.to_string_lossy().into_owned()),
                tx,
                waker,
            )
            .await;
        assert!(matches!(
            refreshed,
            AgentToCore::GitChangeStats { stats: Some(actual), .. } if actual == expected
        ));

        drop(manager);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_recomputes_after_a_debounced_change() {
        let root = repository();
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(91)).unwrap());
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = GitWatchManager::new();
        let initial = manager
            .set(
                ProjectId(7),
                Some(root.to_string_lossy().into_owned()),
                tx,
                waker,
            )
            .await;
        assert!(matches!(
            initial,
            AgentToCore::GitChangeStats {
                stats: Some(GitChangeStats { insertions: 0, .. }),
                ..
            }
        ));

        for index in 0..100 {
            fs::write(root.join("README.md"), format!("changed {index}\n")).unwrap();
        }
        let update =
            tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(4)).unwrap())
                .await
                .unwrap();
        assert!(matches!(
            update,
            AgentToCore::GitChangeStats {
                project: ProjectId(7),
                stats: Some(GitChangeStats { insertions: 1, .. }),
            }
        ));
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let workers = manager
            .repositories
            .get(&canonical_root)
            .map(|watch| watch.workers_started.load(Ordering::Relaxed))
            .unwrap();
        assert!(workers <= 2, "event burst spawned {workers} workers");

        drop(manager);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn projects_in_one_repository_share_one_watcher() {
        let root = repository();
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(92)).unwrap());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut manager = GitWatchManager::new();
        manager
            .set(
                ProjectId(1),
                Some(root.to_string_lossy().into_owned()),
                tx.clone(),
                Arc::clone(&waker),
            )
            .await;
        manager
            .set(
                ProjectId(2),
                Some(nested.to_string_lossy().into_owned()),
                tx.clone(),
                Arc::clone(&waker),
            )
            .await;
        assert_eq!(manager.projects.len(), 2);
        assert_eq!(manager.repositories.len(), 1);

        manager
            .set(ProjectId(1), None, tx.clone(), Arc::clone(&waker))
            .await;
        assert_eq!(manager.repositories.len(), 1);
        manager.set(ProjectId(2), None, tx, waker).await;
        assert!(manager.repositories.is_empty());

        drop(manager);
        fs::remove_dir_all(root).ok();
    }
}
