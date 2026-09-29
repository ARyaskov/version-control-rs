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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec("[abc]{0,3}\n", 0..6).prop_map(|lines| lines.concat())
    }

    proptest! {
        #[test]
        fn one_sided_changes_merge_cleanly(base in text(), other in text()) {
            prop_assert_eq!(three_way_text(&base, &other, &base), (other.clone(), true));
            prop_assert_eq!(three_way_text(&base, &base, &other), (other, true));
        }
    }

    #[test]
    fn property_maps_merge_key_by_key() {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let base = map(&[("a", "1"), ("b", "1")]);
        let mine = map(&[("a", "2"), ("b", "1")]);
        let theirs = map(&[("a", "1"), ("b", "3"), ("c", "new")]);
        let (merged, conflicts) = merge_props(&base, &mine, &theirs);
        assert_eq!(merged, map(&[("a", "2"), ("b", "3"), ("c", "new")]));
        assert!(conflicts.is_empty());
        let (_, conflicts) = merge_props(&base, &map(&[("a", "x")]), &map(&[("a", "y")]));
        // "a" changed differently on both sides; "b" was removed on both.
        assert_eq!(conflicts, vec!["a".to_owned()]);
    }
}
