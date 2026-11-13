//! Files kept as rows: one row per file, keyed by its whole path.
//!
//! A database has no directories, so a host that keeps a body's files in a
//! table needs two rules: which key a path means, and what a directory
//! holds. These are those rules, with no I/O, for any such host to share.

use std::collections::BTreeSet;

/// The key of the file at `path`: its parts joined by `/`, with no leading
/// slash, no empty part and no `.` or `..`.
///
/// Every path starts from one root, so `/a/b`, `a/b` and `./a//b` are one
/// file. A `..` steps up, and a path that steps above the root has no key.
pub fn key(path: &str) -> Result<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("the path leaves the directory files start from".into());
                }
            }
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

/// What is directly in the directory whose key is `directory`, given every
/// file's key. A directory's name ends in `/`. `None` when nothing is under
/// the directory, so it does not exist, unless it is the root.
pub fn children<'a>(
    directory: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> Option<Vec<String>> {
    let mut names = BTreeSet::new();
    for key in keys {
        let under = if directory.is_empty() {
            key
        } else {
            let Some(under) = key
                .strip_prefix(directory)
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                continue;
            };
            under
        };
        match under.split_once('/') {
            Some((child, _)) => names.insert(format!("{child}/")),
            None => names.insert(under.to_owned()),
        };
    }
    (!names.is_empty() || directory.is_empty()).then(|| names.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_file_has_one_key_however_its_path_is_written() {
        for path in [
            "a/b.txt",
            "/a/b.txt",
            "./a//b.txt",
            "a/c/../b.txt",
            "a/b.txt/",
        ] {
            assert_eq!(key(path).unwrap(), "a/b.txt", "{path}");
        }
        assert_eq!(key("").unwrap(), "");
        assert_eq!(key("/").unwrap(), "");
        assert!(key("../secrets").is_err());
        assert!(key("a/../../b").is_err());
    }

    #[test]
    fn a_directory_holds_what_is_directly_under_it() {
        let keys = ["b.txt", "a/one.txt", "a/deep/two.txt", "ab/three.txt"];
        assert_eq!(children("", keys).unwrap(), ["a/", "ab/", "b.txt"]);
        assert_eq!(children("a", keys).unwrap(), ["deep/", "one.txt"]);
        assert_eq!(children("a/deep", keys).unwrap(), ["two.txt"]);
        assert_eq!(children("nope", keys), None);
        assert_eq!(children("b.txt", keys), None);
        assert_eq!(children("", []).unwrap(), Vec::<String>::new());
    }
}
