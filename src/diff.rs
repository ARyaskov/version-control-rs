use similar::TextDiff;

/// Render a proper unified diff: `--- / +++` headers followed by `@@ a,b c,d @@`
/// hunks with 3 lines of context (only changed regions, not the whole file).
pub fn unified_diff(old_text: &str, new_text: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old_text, new_text);
    diff.unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}
