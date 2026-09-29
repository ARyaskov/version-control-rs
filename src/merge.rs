//! Three-way merging of text and property maps.

use std::collections::{BTreeMap, BTreeSet};

use diffy::merge as diffy_merge;

/// Three-way text merge: `(merged, true)` when the edits do not overlap,
/// `(text with conflict markers, false)` otherwise.
pub fn three_way_text(base: &str, ours: &str, theirs: &str) -> (String, bool) {
    match diffy_merge(base, ours, theirs) {
        Ok(text) => (text, true),
        Err(conflict) => (conflict, false),
    }
}

/// Three-way merge of property maps, key by key. Returns the merged map and
/// the keys changed differently on both sides (the local value is kept).
pub fn merge_props(
    base: &BTreeMap<String, String>,
    mine: &BTreeMap<String, String>,
    theirs: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let keys: BTreeSet<&String> = base
        .keys()
        .chain(mine.keys())
        .chain(theirs.keys())
        .collect();
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    for key in keys {
        let (b, m, t) = (base.get(key), mine.get(key), theirs.get(key));
        let value = if m == t || t == b {
            m
        } else if m == b {
            t
        } else {
            conflicts.push(key.clone());
            m
        };
        if let Some(v) = value {
            merged.insert(key.clone(), v.clone());
        }
    }
    (merged, conflicts)
}
