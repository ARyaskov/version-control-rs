use similar::{ChangeTag, TextDiff};

pub fn unified_diff(old_text: &str, new_text: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old_text, new_text);
    let mut out = String::new();
    out.push_str(&format!("--- {}\n+++ {}\n", old_label, new_label));

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => out.push('-'),
            ChangeTag::Insert => out.push('+'),
            ChangeTag::Equal => out.push(' '),
        }
        out.push_str(change.to_string().as_str());
    }

    out
}
