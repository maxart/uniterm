//! Wrap and index a conversation once for its current terminal width.
//! A post owns one body buffer; scrolling materializes just the viewport.

use super::{author_label, card_label_width, metadata, CardLine, InboxPost};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Reused by painting, scrolling, and hit-testing until the thread or its width changes.
#[derive(Default)]
pub(super) struct ThreadLayout {
    pub(super) width: usize,
    pub(super) rows: usize,
    posts: Vec<PostLayout>,
    // Navigable rows in visual order, paired with selection identity across refreshes.
    pub(super) links: Vec<(usize, (u64, u64))>,
    targets: BTreeSet<u64>,
}

struct PostLayout {
    start: usize,
    header: Vec<CardLine>,
    body: WrappedBody,
    footer: Vec<CardLine>,
}

struct WrappedBody {
    text: String,
    // The attach frame is bounded, and a board post is at most 8 KiB
    // before four-space tab expansion. One offset represents a visual row.
    starts: Vec<u32>,
}

impl WrappedBody {
    fn new(body: &str, width: usize) -> Self {
        let mut text = String::with_capacity(body.len());
        for c in body.chars() {
            match c {
                '\t' => text.push_str("    "),
                '\n' => text.push(c),
                c if c.is_control() => text.push(' '),
                c => text.push(c),
            }
        }

        let mut starts = Vec::new();
        if !text.is_empty() {
            starts.push(0);
            let width = width.max(1);
            let mut used = 0;
            if text.is_ascii() {
                for (at, &byte) in text.as_bytes().iter().enumerate() {
                    if byte == b'\n' {
                        starts.push((at + 1) as u32);
                        used = 0;
                    } else {
                        if used == width {
                            starts.push(at as u32);
                            used = 0;
                        }
                        used += 1;
                    }
                }
            } else {
                for (at, cluster) in text.grapheme_indices(true) {
                    if cluster == "\n" {
                        starts.push((at + 1) as u32);
                        used = 0;
                    } else {
                        let cells = cluster.width();
                        if used > 0 && used + cells > width {
                            starts.push(at as u32);
                            used = 0;
                        }
                        used += cells;
                    }
                }
            }
        }
        // A final newline terminates the final row. Additional newlines still
        // represent explicit blank rows (including before CLI --ref links).
        if text.ends_with('\n') {
            starts.pop();
        }
        Self { text, starts }
    }

    fn line(&self, row: usize) -> Option<&str> {
        let start = *self.starts.get(row)? as usize;
        let end = self
            .starts
            .get(row + 1)
            .map_or(self.text.len(), |i| *i as usize);
        Some(self.text[start..end].trim_end_matches('\n'))
    }
}

impl ThreadLayout {
    /// Build row and link indexes in the same pass so navigation matches the painted cells.
    pub(super) fn build(
        width: usize,
        posts: &[InboxPost],
        parents: &BTreeMap<u64, u64>,
        threads: &BTreeMap<u64, Vec<InboxPost>>,
    ) -> Self {
        let mut layout = Self {
            width,
            ..Self::default()
        };
        for post in posts {
            let mut title = format!("#{} · {}", post.id, author_label(post.author));
            if let Some(parent) = parents.get(&post.id).filter(|id| **id != post.thread) {
                title.push_str(&format!(" · reply to #{parent}"));
            }
            let mut header = vec![CardLine::Top(title)];
            let mut context = Vec::new();
            if let Some(to) = post.to {
                context.push(format!("To {}", author_label(to)));
            }
            if let Some(task) = post.task {
                context.push(format!("Task #{task}"));
            }
            if let Some(owner) = post.claimed_by {
                context.push(format!("Claimed by {}", author_label(owner)));
            }
            if !context.is_empty() {
                header.extend(
                    crate::overlay::wrap_text(&context.join("  ·  "), width)
                        .into_iter()
                        .map(CardLine::Note),
                );
            }

            // A paragraph containing only CLI --ref links is already shown
            // by its actionable rows. Preserve all other prose verbatim.
            let (prose, tail) = post.body.rsplit_once("\n\n").unwrap_or(("", &post.body));
            let bare_links = !tail.trim().is_empty()
                && tail.split_whitespace().all(|word| {
                    word.strip_prefix("thread:")
                        .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
                        .and_then(|id| id.parse::<u64>().ok())
                        .is_some_and(|id| id != 0)
                });
            let body = WrappedBody::new(if bare_links { prose } else { &post.body }, width);
            let start = layout.rows;
            layout.rows += header.len() + body.starts.len();
            let mut footer = Vec::new();
            let mut seen = BTreeSet::new();
            for target in uniterm_core::inbox::thread_mentions(&post.body) {
                if seen.insert(target) {
                    if target == post.thread {
                        footer.push(CardLine::Note(format!("thread:{target} · this discussion")));
                        continue;
                    }
                    layout.targets.insert(target);
                    if threads.contains_key(&target) {
                        layout
                            .links
                            .push((layout.rows + footer.len(), (post.id, target)));
                    }
                    footer.push(CardLine::Link {
                        post: post.id,
                        target,
                    });
                }
            }
            footer.push(CardLine::Bottom(metadata(
                post,
                card_label_width(width + 4),
            )));
            layout.rows += footer.len();
            layout.posts.push(PostLayout {
                start,
                header,
                body,
                footer,
            });
        }
        layout
    }

    /// Both live and unavailable targets can change how this conversation is presented.
    pub(super) fn references(&self, target: u64) -> bool {
        self.targets.contains(&target)
    }

    fn post_at(&self, row: usize) -> Option<(&PostLayout, usize)> {
        let i = self
            .posts
            .partition_point(|p| p.start <= row)
            .checked_sub(1)?;
        let post = &self.posts[i];
        Some((post, row - post.start))
    }

    /// Pointer navigation resolves the footer row without materializing any post body.
    pub(super) fn link_at(&self, row: usize) -> Option<(u64, u64)> {
        let (post, local) = self.post_at(row)?;
        let i = local.checked_sub(post.header.len() + post.body.starts.len())?;
        post.footer.get(i)?.link()
    }

    /// Limit row materialization to the visible part of this snapshot.
    pub(super) fn visible(
        &self,
        from: usize,
        count: usize,
    ) -> impl Iterator<Item = Cow<'_, CardLine>> {
        (from..from.saturating_add(count).min(self.rows)).filter_map(|row| {
            let (post, local) = self.post_at(row)?;
            if let Some(line) = post.header.get(local) {
                return Some(Cow::Borrowed(line));
            }
            let body_row = local - post.header.len();
            if let Some(line) = post.body.line(body_row) {
                return Some(Cow::Owned(CardLine::Body(line.to_owned())));
            }
            post.footer
                .get(body_row - post.body.starts.len())
                .map(Cow::Borrowed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_body_rows_preserve_indentation_controls_and_graphemes() {
        for (body, width, expected) in [
            ("  界x\n\n\n", 4, vec!["  界", "x", "", ""]),
            ("e\u{301}👩‍💻ab", 3, vec!["e\u{301}👩‍💻", "ab"]),
            ("\tA\u{7}B", 3, vec!["   ", " A ", "B"]),
            ("longword", 3, vec!["lon", "gwo", "rd"]),
            ("line\n", 4, vec!["line"]),
            ("\n", 4, vec![""]),
            ("", 4, vec![]),
        ] {
            let wrapped = WrappedBody::new(body, width);
            assert_eq!(wrapped.starts.len(), expected.len());
            let lines = (0..wrapped.starts.len())
                .map(|row| wrapped.line(row).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(lines, expected);
        }
    }
}
