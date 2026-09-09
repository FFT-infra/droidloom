//! Treat upstream ZIP/tar inputs only as data, with no archive-directed writes.
use crate::util::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

pub const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const MAX_LIST: u64 = 2 * 1024 * 1024;
const MAX_MEMBERS: usize = 8192;

/// Only regular files and directories are accepted, including the outer ZIP.
/// Members are copied to caller-selected paths via stdout, never extracted by name.
pub fn list(archive: &Path) -> Result<BTreeMap<String, bool>> {
    let names = String::from_utf8(capture(
        Command::new("bsdtar").arg("-tf").arg(archive),
        MAX_LIST,
    )?)?;
    let details = String::from_utf8(capture(
        Command::new("bsdtar").arg("-tvf").arg(archive),
        MAX_LIST,
    )?)?;
    parse_listing(&names, &details)
}

fn parse_listing(names: &str, details: &str) -> Result<BTreeMap<String, bool>> {
    let names: Vec<_> = names.lines().collect();
    let details: Vec<_> = details.lines().collect();
    if names.is_empty() || names.len() > MAX_MEMBERS || names.len() != details.len() {
        return Err("invalid or oversized archive member list".into());
    }
    let mut result = BTreeMap::new();
    for (name, detail) in names.into_iter().zip(details) {
        let directory = match detail.as_bytes().first() {
            Some(b'd') => true,
            Some(b'-') => false,
            _ => return Err("archive links, devices and special files are unsupported".into()),
        };
        let normalized = if directory {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        // Excludes whitespace, escapes, globs, traversal and ambiguous spellings.
        if !normal_relative(normalized) || result.insert(normalized.to_owned(), directory).is_some()
        {
            return Err(format!("unsafe or duplicate archive path: {name:?}").into());
        }
    }
    for name in result.keys() {
        let mut parent = Path::new(name).parent();
        while let Some(path) = parent {
            if result.get(path.to_str().ok_or("non-UTF8 archive path")?) == Some(&false) {
                return Err("archive file is also used as a directory".into());
            }
            parent = path.parent();
        }
    }
    Ok(result)
}

pub fn member(archive: &Path, name: &str, maximum: u64) -> Result<Vec<u8>> {
    if !normal_relative(name) {
        return Err("invalid archive member".into());
    }
    capture(
        Command::new("bsdtar")
            .arg("-xOf")
            .arg(archive)
            .arg("--")
            .arg(name),
        maximum,
    )
}

pub fn copy_member(archive: &Path, name: &str, destination: &Path, maximum: u64) -> Result<()> {
    if !normal_relative(name) {
        return Err("invalid archive member".into());
    }
    fs::create_dir_all(destination.parent().ok_or("file has no parent")?)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    stream(
        Command::new("bsdtar")
            .arg("-xOf")
            .arg(archive)
            .arg("--")
            .arg(name),
        &mut file,
        maximum,
    )?;
    Ok(())
}

pub fn properties(text: &str) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line.split_once('=').ok_or("invalid property line")?;
        if result.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err("duplicate property".into());
        }
    }
    Ok(result)
}

pub fn ensure_payload(members: &BTreeMap<String, bool>, prefix: &str) -> Result<()> {
    let ancestors: BTreeSet<_> = Path::new(prefix.trim_end_matches('/'))
        .ancestors()
        .filter_map(|p| p.to_str())
        .collect();
    if members.keys().any(|path| {
        !(path.starts_with(prefix) || members[path] && ancestors.contains(path.as_str()))
    }) {
        return Err(
            "payload contains files outside the selected architecture/SDK system tree".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_aliases_duplicates_links_and_parent_conflicts() {
        for (names, details) in [
            ("../outside\n", "-rw-r--r-- file\n"),
            ("/outside\n", "-rw-r--r-- file\n"),
            ("foo\\nbar\n", "-rw-r--r-- file\n"),
            ("file\nfile\n", "-rw-r--r--\n-rw-r--r--\n"),
            ("link\n", "lrwxrwxrwx link -> /etc\n"),
            ("link\n", "hrw-r--r-- link link to other\n"),
            ("dir\ndir/file\n", "-rw-r--r--\n-rw-r--r--\n"),
        ] {
            assert!(parse_listing(names, details).is_err(), "{names}");
        }
        assert!(parse_listing("dir/\ndir/file\n", "drwxr-xr-x\n-rw-r--r--\n").is_ok());
    }
    #[test]
    fn real_archive_reader_rejects_symlinks_and_bounds_decompression() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), [0; 4096]).unwrap();
        let archive = dir.path().join("archive.tar");
        run(Command::new("bsdtar")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path())
            .arg("file"))
        .unwrap();
        assert_eq!(list(&archive).unwrap().get("file"), Some(&false));
        assert!(member(&archive, "file", 128).is_err());
        assert_eq!(member(&archive, "file", 4096).unwrap().len(), 4096);
        symlink("/etc", dir.path().join("link")).unwrap();
        run(Command::new("bsdtar")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path())
            .arg("link"))
        .unwrap();
        assert!(list(&archive).is_err());
    }
}
