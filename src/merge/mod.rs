#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    Recursive,
    Ours,
    Theirs,
}

pub fn three_way_text(base: &str, ours: &str, theirs: &str) -> String {
    if ours == base {
        return theirs.to_owned();
    }
    if theirs == base {
        return ours.to_owned();
    }
    if ours == theirs {
        return ours.to_owned();
    }

    format!(
        "<<<<<<< .mine\n{}\n||||||| .rOLD\n{}\n=======\n{}\n>>>>>>> .rNEW\n",
        ours, base, theirs
    )
}
