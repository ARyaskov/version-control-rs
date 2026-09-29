//! Repository-form normalization of file content: binary detection,
//! end-of-line styles and keyword expansion, driven by svn properties.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

/// Apply repository-form normalization (eol, keyword contraction, symlink/binary
/// handling) to raw working bytes, returning the normalized bytes, the node
/// property set, and whether the content is treated as binary.
///
/// Node properties are the explicit ones, except that `svn:executable` and
/// `svn:special` mirror the file itself (on Unix, where the filesystem can
/// express them): `chmod -x` removes `svn:executable`, replacing a symlink by
/// a file removes `svn:special`. Inherited properties steer normalization but
/// are never recorded on the node.
pub(crate) fn repo_form(
    mut raw: Vec<u8>,
    is_symlink: bool,
    file_props: &BTreeMap<String, String>,
    inherited: &BTreeMap<String, String>,
    executable: bool,
) -> (Vec<u8>, BTreeMap<String, String>, bool) {
    let detected_binary = is_binary_content(&raw);
    let props = node_props(file_props, is_symlink, executable);
    let effective = with_inherited(&props, inherited);
    let is_binary = effective_is_binary(detected_binary, &effective);
    // Only normalize line endings when svn:eol-style is set; without it,
    // content is stored byte-for-byte (svn semantics).
    if !is_binary
        && !has_svn_prop(&effective, "svn:special")
        && has_svn_prop(&effective, "svn:eol-style")
        && let Ok(text) = std::str::from_utf8(&raw)
    {
        raw = normalize_eol(text, effective.get("svn:eol-style")).into_bytes();
    }
    if has_svn_prop(&effective, "svn:keywords")
        && !has_svn_prop(&effective, "svn:special")
        && let Ok(text) = std::str::from_utf8(&raw)
    {
        raw = contract_keywords(text, &effective).into_bytes();
    }
    (raw, props, is_binary)
}

/// Node properties of a working file: the explicit ones, with `svn:executable`
/// and `svn:special` mirroring the file itself where the filesystem can
/// express them (see [`repo_form`]).
pub(crate) fn node_props(
    file_props: &BTreeMap<String, String>,
    is_symlink: bool,
    executable: bool,
) -> BTreeMap<String, String> {
    let mut props = file_props.clone();
    if cfg!(unix) {
        if executable {
            props.insert("svn:executable".to_owned(), "*".to_owned());
        } else {
            props.remove("svn:executable");
        }
    }
    if is_symlink {
        props.insert("svn:special".to_owned(), "*".to_owned());
    } else if cfg!(unix) {
        props.remove("svn:special");
    }
    props
}

/// Node properties overlaid with inherited ones (node properties win).
pub(crate) fn with_inherited(
    props: &BTreeMap<String, String>,
    inherited: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = props.clone();
    for (k, v) in inherited {
        out.entry(k.clone()).or_insert_with(|| v.clone());
    }
    out
}

/// Heuristic detection of unresolved SVN conflict markers in committed content.
pub(crate) fn has_conflict_markers(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut start = false;
    let mut end = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            start = true;
        } else if line.starts_with(">>>>>>>") {
            end = true;
        }
    }
    start && end
}

/// Number of leading bytes inspected for binary detection (svn uses a similar
/// small window rather than scanning whole files).
pub(crate) const BINARY_SNIFF_LEN: usize = 8192;

pub(crate) fn is_binary_content(bytes: &[u8]) -> bool {
    let n = bytes.len().min(BINARY_SNIFF_LEN);
    let window = &bytes[..n];
    if window.contains(&0) {
        return true;
    }
    match std::str::from_utf8(window) {
        Ok(_) => false,
        // When the file is larger than the window, a None error_len means the
        // window was cut mid-character — that is a truncation artifact, not a
        // binary byte, so treat it as text.
        Err(e) => !(n < bytes.len() && e.error_len().is_none()),
    }
}

pub(crate) fn has_svn_prop(props: &BTreeMap<String, String>, name: &str) -> bool {
    props.get(name).is_some_and(|v| !v.trim().is_empty())
}

pub(crate) fn keyword_names(props: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(value) = props.get("svn:keywords") {
        for name in value.split(|c: char| c.is_whitespace() || c == ',') {
            let n = name.trim();
            if !n.is_empty() {
                out.insert(n.to_owned());
            }
        }
    }
    out
}

pub(crate) fn contract_keywords(text: &str, props: &BTreeMap<String, String>) -> String {
    let mut out = text.to_owned();
    for k in keyword_names(props) {
        out = collapse_keyword(&out, &k);
    }
    out
}

pub(crate) fn effective_is_binary(detected_binary: bool, props: &BTreeMap<String, String>) -> bool {
    if let Some(mt) = props.get("svn:mime-type") {
        let mt = mt.trim().to_ascii_lowercase();
        if mt.starts_with("text/") || mt == "application/xml" || mt == "application/json" {
            return false;
        }
        if !mt.is_empty() {
            return true;
        }
    }
    detected_binary
}

pub(crate) fn normalize_eol(text: &str, eol_style: Option<&String>) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match eol_style.map(|s| s.as_str()) {
        Some("CRLF") | Some("crlf") => normalized.replace('\n', "\r\n"),
        Some("CR") | Some("cr") => normalized.replace('\n', "\r"),
        _ => normalized,
    }
}

pub(crate) fn apply_eol_style_for_working(text: &str, eol_style: Option<&String>) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match eol_style.map(|s| s.as_str()) {
        Some("LF") | Some("lf") => normalized,
        Some("CRLF") | Some("crlf") => normalized.replace('\n', "\r\n"),
        Some("CR") | Some("cr") => normalized.replace('\n', "\r"),
        Some("native") | Some("NATIVE") => {
            #[cfg(windows)]
            {
                normalized.replace('\n', "\r\n")
            }
            #[cfg(not(windows))]
            {
                normalized
            }
        }
        _ => normalized,
    }
}

/// Longest expanded keyword svn recognizes (`$Name: value $`), in bytes.
pub(crate) const MAX_KEYWORD_LEN: usize = 255;

/// Contract every expanded `$Name: ... $` back to `$Name$`. The closing `$`
/// must be on the same line and within [`MAX_KEYWORD_LEN`] bytes (svn's
/// rule); anything else is ordinary text and is left untouched, so a stray
/// `$Rev:` can never swallow the content up to some later `$`.
pub(crate) fn collapse_keyword(input: &str, name: &str) -> String {
    let needle = format!("${name}:");
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while let Some(pos) = input[i..].find(&needle) {
        let start = i + pos;
        let after = start + needle.len();
        let line_end = input[after..].find('\n').map_or(input.len(), |n| after + n);
        let closing = input[after..line_end].find('$').map(|n| after + n);
        match closing {
            Some(end) if end + 1 - start <= MAX_KEYWORD_LEN => {
                out.push_str(&input[i..start]);
                out.push_str(&format!("${name}$"));
                i = end + 1;
            }
            _ => {
                out.push_str(&input[i..after]);
                i = after;
            }
        }
    }
    out.push_str(&input[i..]);
    out
}

pub(crate) fn expand_keywords(
    text: &str,
    props: &BTreeMap<String, String>,
    rev: i64,
    author: &str,
    date: DateTime<Utc>,
) -> String {
    let mut out = text.to_owned();
    for key in keyword_names(props) {
        let value = match key.as_str() {
            "Rev" => rev.to_string(),
            "Author" => author.to_owned(),
            "Date" => date.to_rfc3339(),
            "Id" => format!("{rev} {author} {}", date.to_rfc3339()),
            _ => continue,
        };
        out = out.replace(&format!("${key}$"), &format!("${key}: {value} $"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_collapse_is_line_bounded() {
        assert_eq!(collapse_keyword("id $Rev: 12 $ end", "Rev"), "id $Rev$ end");
        // An unterminated keyword must not consume the following lines.
        let text = "price $Rev: none\nkeep this line\ncost $5\n";
        assert_eq!(collapse_keyword(text, "Rev"), text);
        // Over-long "values" are not keywords either.
        let long = format!("$Rev: {} $", "x".repeat(300));
        assert_eq!(collapse_keyword(&long, "Rev"), long);
        // Several keywords on one line.
        assert_eq!(
            collapse_keyword("$Rev: 1 $ and $Rev: 2 $", "Rev"),
            "$Rev$ and $Rev$"
        );
    }
}
