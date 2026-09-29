//! Line attribution (blame).

use super::*;

impl Repository {
    pub fn blame_file(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>> {
        let target_rev = match revision {
            Some(spec) => self.resolve_revision_spec(spec)?,
            None => self.wcdb()?.head_revision()?,
        };
        if target_rev <= 0 {
            return Ok(Vec::new());
        }
        let mut lines = self.blame_file_at_revision(path, target_rev)?;
        self.apply_merge_awareness(path, target_rev, &mut lines)?;
        Ok(lines)
    }

    /// Line attribution of `path` at `target_rev`. Only revisions whose change
    /// set mentions the path are visited (and only the trees along the path
    /// are read); a deletion ends the file's history, so a later re-addition
    /// starts fresh.
    pub(super) fn blame_file_at_revision(
        &self,
        path: &str,
        target_rev: i64,
    ) -> Result<Vec<BlameLine>> {
        let mut previous_lines: Vec<String> = Vec::new();
        let mut attributions: Vec<BlameLine> = Vec::new();
        let mut seen_any = false;

        for touch in self.wcdb()?.revisions_touching(path, target_rev)? {
            let rev = touch.rev;
            let Some(file) = self.file_entry_at_revision(rev, path)? else {
                previous_lines.clear();
                attributions.clear();
                seen_any = false;
                continue;
            };
            if file.is_binary {
                return Err(VcsError::CommitNotFound(format!(
                    "blame for binary file r{rev}:{path}"
                )));
            }

            let text = String::from_utf8_lossy(&self.read_blob(&file.blob_id)?).to_string();
            let new_lines: Vec<String> = text.lines().map(|s| s.to_owned()).collect();

            if !seen_any {
                attributions = match (&file.copy_from_path, file.copy_from_rev) {
                    (Some(src_path), Some(src_rev)) if src_rev > 0 => {
                        let src_blame = self.blame_file_at_revision(src_path, src_rev)?;
                        let src_lines: Vec<String> =
                            src_blame.iter().map(|l| l.content.clone()).collect();
                        apply_diff_to_blame(
                            &src_blame,
                            &src_lines,
                            &new_lines,
                            &touch.author,
                            rev,
                            touch.created_at,
                        )
                    }
                    _ => base_blame_for_lines(&new_lines, rev, &touch.author, touch.created_at),
                };
                previous_lines = new_lines;
                seen_any = true;
                continue;
            }

            attributions = apply_diff_to_blame(
                &attributions,
                &previous_lines,
                &new_lines,
                &touch.author,
                rev,
                touch.created_at,
            );
            previous_lines = new_lines;
        }

        Ok(attributions)
    }

    /// Credit merged lines to the revision that wrote them. A line attributed
    /// to a merge commit that also exists — aligned by diff, not by line
    /// number — in the merged revision's version of the file takes that
    /// version's attribution.
    pub(super) fn apply_merge_awareness(
        &self,
        path: &str,
        target_rev: i64,
        lines: &mut [BlameLine],
    ) -> Result<()> {
        for (merge_rev, merged_rev, source_path) in self.wcdb()?.merge_edges_up_to(target_rev)? {
            if !path_in_scope(path, &source_path) || !lines.iter().any(|l| l.revision == merge_rev)
            {
                continue;
            }
            let Ok(source) = self.blame_file_at_revision(path, merged_rev) else {
                continue;
            };
            let source_text: Vec<&str> = source.iter().map(|l| l.content.as_str()).collect();
            let current_text: Vec<String> = lines.iter().map(|l| l.content.clone()).collect();
            let current_text: Vec<&str> = current_text.iter().map(String::as_str).collect();
            for op in capture_diff_slices(Algorithm::Myers, &source_text, &current_text) {
                if op.tag() != DiffTag::Equal {
                    continue;
                }
                for (src_idx, cur_idx) in op.old_range().zip(op.new_range()) {
                    let line = &mut lines[cur_idx];
                    if line.revision == merge_rev {
                        let origin = &source[src_idx];
                        line.revision = origin.revision;
                        line.author = origin.author.clone();
                        line.created_at = origin.created_at;
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) fn base_blame_for_lines(
    lines: &[String],
    rev: i64,
    author: &str,
    created_at: chrono::DateTime<Utc>,
) -> Vec<BlameLine> {
    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| BlameLine {
            line_no: idx + 1,
            revision: rev,
            author: author.to_owned(),
            content: line.clone(),
            created_at,
        })
        .collect()
}

pub(super) fn apply_diff_to_blame(
    prior: &[BlameLine],
    old_lines: &[String],
    new_lines: &[String],
    author: &str,
    rev: i64,
    created_at: chrono::DateTime<Utc>,
) -> Vec<BlameLine> {
    let mut next = Vec::new();
    let ops = capture_diff_slices(Algorithm::Myers, old_lines, new_lines);
    for op in ops {
        match op.tag() {
            DiffTag::Equal => {
                for idx in op.old_range() {
                    if let Some(prev) = prior.get(idx) {
                        next.push(prev.clone());
                    }
                }
            }
            DiffTag::Delete => {}
            DiffTag::Insert | DiffTag::Replace => {
                for idx in op.new_range() {
                    next.push(BlameLine {
                        line_no: idx + 1,
                        revision: rev,
                        author: author.to_owned(),
                        content: new_lines.get(idx).cloned().unwrap_or_default(),
                        created_at,
                    });
                }
            }
        }
    }
    for (idx, item) in next.iter_mut().enumerate() {
        item.line_no = idx + 1;
        item.content = new_lines.get(idx).cloned().unwrap_or_default();
    }
    next
}
