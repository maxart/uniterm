//! A bounded read-only inventory. All facts come from this server's owning maps.
use super::*;
use uniterm_proto::{overwatch::Fact, ControlResult};

const MAX_PAGE: usize = 64;

struct Page {
    offset: usize,
    limit: usize,
    total: usize,
    truncated: bool,
    records: Vec<Fact>,
}
impl Page {
    fn new(offset: usize, limit: u16) -> Self {
        Self {
            offset,
            limit: usize::from(limit).clamp(1, MAX_PAGE),
            total: 0,
            truncated: false,
            records: Vec::new(),
        }
    }
    fn push(&mut self, make: impl FnOnce(&mut bool) -> Fact) {
        if self.total >= self.offset && self.records.len() < self.limit {
            self.records.push(make(&mut self.truncated));
        }
        self.total = self.total.saturating_add(1);
    }
    fn section<T>(
        &mut self,
        count: usize,
        iter: impl Iterator<Item = T>,
        mut make: impl FnMut(T, &mut bool) -> Fact,
    ) {
        let end = self.total.saturating_add(count);
        if self.offset < end && self.records.len() < self.limit {
            let skip = self.offset.saturating_sub(self.total);
            for item in iter.skip(skip).take(self.limit - self.records.len()) {
                self.records.push(make(item, &mut self.truncated));
            }
        }
        self.total = end;
    }

    fn next(&self) -> Option<usize> {
        let next = self.offset.saturating_add(self.records.len());
        (next < self.total).then_some(next)
    }
}

fn safe(value: &str, max: usize, truncated: &mut bool) -> String {
    let mut chars = value.chars().filter(|c| !c.is_control());
    let text: String = chars.by_ref().take(max).collect();
    *truncated |= chars.next().is_some();
    text
}

impl Server {
    pub(super) fn overwatch_context_page(&self, offset: usize, limit: u16) -> ControlResult {
        let mut page = Page::new(offset, limit);
        for project in &self.projects {
            page.push(|truncated| Fact::Project {
                id: project.id,
                name: safe(&project.name, 128, truncated),
                root: safe(&project.root, 512, truncated),
                active: project.id == self.active_project,
            });
        }
        for (index, tab) in self.windows.iter().enumerate() {
            page.push(|truncated| Fact::Tab {
                anchor: tab.active,
                project: tab.project,
                name: safe(
                    // Automatic labels can contain provider-authored private
                    // text. Only an explicit Tab name is public metadata.
                    tab.name.as_deref().unwrap_or("Unnamed Tab"),
                    128,
                    truncated,
                ),
                parent: tab.stack_parent,
                active: index == self.active_window,
            });
            for id in tab.layout.pane_ids() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                page.push(|truncated| Fact::Pane {
                    id,
                    project: tab.project,
                    tab: tab.active,
                    cwd: safe(
                        &pane
                            .cwd
                            .as_ref()
                            .map(|path| path.to_string_lossy())
                            .unwrap_or_default(),
                        512,
                        truncated,
                    ),
                    recovery: pane.recovery.is_some(),
                });
                if let Some(agent) = &pane.agent {
                    page.push(|truncated| {
                        let owner = self.run_graph.active_for_pane(id);
                        Fact::Agent {
                            pane: id,
                            project: tab.project,
                            provider: safe(&agent.id, 128, truncated),
                            status: agent.status,
                            invocation: self.timeline_invocations.get(&id).copied(),
                            run: owner.map(|(run, _)| run),
                            role: owner.map(|(_, role)| role),
                        }
                    });
                }
            }
        }
        page.section(self.tasks.len(), self.tasks.records(), |task, truncated| {
            Fact::Task {
                id: task.id,
                // Automatic titles can be the entire submitted prompt. Their
                // provenance remains private even after later observations.
                title: if task.context.observed.is_some() {
                    format!("Task #{}", task.id)
                } else {
                    safe(&task.title, 256, truncated)
                },
                status: task.status,
                project: task.context.project,
                pane: task.context.pane,
                source: task.context.source_task,
                updated: task.updated,
            }
        });
        let mut runs = 0;
        for run in self.run_graph.runs() {
            runs += 1;
            page.push(|truncated| Fact::Run {
                id: run.id,
                project: run.project,
                parent: run.parent,
                task: run.task_id,
                title: safe(&run.title, 256, truncated),
                status: run.status,
                shape: run.kind,
            });
            for id in &run.roles {
                if let Some(role) = self.run_graph.role(*id) {
                    page.push(|truncated| Fact::Role {
                        id: role.id,
                        run: run.id,
                        pane: role.pane,
                        name: safe(&role.name, 128, truncated),
                        provider: safe(&role.provider, 128, truncated),
                        activation: role.activation.as_ref().map(|a| a.id),
                        active: role.activation.as_ref().is_some_and(|a| a.active),
                    });
                }
            }
        }
        for artifact in self.artifacts.artifacts() {
            page.push(|truncated| Fact::Artifact {
                id: artifact.id,
                project: artifact.project,
                run: artifact.producer_run,
                role: artifact.producer_role,
                path: safe(&artifact.path, 512, truncated),
                status: artifact.status,
                artifact_kind: artifact.kind,
                size: artifact.size,
                digest: safe(&artifact.digest, 64, truncated),
            });
        }
        for waiting in self.waiting.items() {
            page.push(|_| Fact::Waiting {
                id: waiting.id,
                pane: waiting.pane,
                waiting_kind: waiting.kind,
            });
        }
        ControlResult::OverwatchContext {
            workspace: self.name.clone(),
            sequence: self.log.current_sequence(),
            active_project: self.active_project,
            next_offset: page.next(),
            total: page.total,
            offset,
            records: page.records,
            truncated_text: page.truncated,
            private_content_excluded: true,
            tasks: self.tasks.len(),
            runs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_pages_are_bounded_and_sanitize_only_selected_records() {
        let mut page = Page::new(2, u16::MAX);
        for id in 0..1000 {
            page.push(|truncated| Fact::Project {
                id: ProjectId(id),
                name: safe(&"界\x1b".repeat(2048), 128, truncated),
                root: safe(&"😀".repeat(4096), 512, truncated),
                active: false,
            });
        }
        assert_eq!(page.records.len(), MAX_PAGE);
        assert_eq!(page.total, 1000);
        assert_eq!(page.next(), Some(66));
        assert!(page.truncated);
        let bytes = serde_json::to_vec(&page.records).unwrap();
        assert!(bytes.len() < uniterm_proto::CONTROL_MAX_FRAME_BYTES as usize / 2);
        assert!(!String::from_utf8(bytes).unwrap().contains("\\u001b"));
        let mut empty = Page::new(usize::MAX, 0);
        empty.push(|_| panic!("out-of-range page must not materialize private state"));
        assert!(empty.records.is_empty());
        assert_eq!(empty.next(), None);
    }
}
