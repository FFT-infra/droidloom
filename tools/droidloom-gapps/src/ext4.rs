//! Rootless, additive ext4 derivation. Only a private image copy is ever edited.
use crate::util::*;
use droidloom_contracts::gapps::FileDigest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::Command,
};

const MAX_ENTRIES: usize = 32_768;
const MAX_FILE: u64 = 2 * 1024 * 1024 * 1024;
const EPOCH: u64 = 1_230_768_000;

fn check(image: &Path) -> Result<()> {
    let report = text(Command::new("e2fsck").args(["-f", "-n"]).arg(image))?;
    // e2fsck can return success for some declined timestamp repairs. A clean
    // candidate must not depend on a repair prompt, even with exit status 0.
    if report.contains("Fix?") || report.contains("UNEXPECTED INCONSISTENCY") {
        return Err(format!("ext4 image requires repair: {}\n{report}", image.display()).into());
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: String,
    xattrs: String,
    content: Option<FileDigest>,
    link: Option<String>,
}

pub fn is_ext4(image: &Path) -> Result<bool> {
    let mut file = fs::File::open(image)?;
    file.seek(SeekFrom::Start(1080))?;
    let mut magic = [0; 2];
    file.read_exact(&mut magic)?;
    Ok(magic == [0x53, 0xef])
}

fn quote(path: &Path) -> Result<String> {
    let path = path.to_str().ok_or("non-UTF8 debugfs path")?;
    if path.bytes().any(|b| b < 32 || b == b'"' || b == b'\\') {
        return Err("unsupported debugfs path spelling".into());
    }
    Ok(format!("\"{path}\""))
}

fn debug(image: &Path, request: &str) -> Result<String> {
    text(Command::new("debugfs").arg("-R").arg(request).arg(image))
}

pub fn read_file(image: &Path, path: &str, maximum: u64) -> Result<Vec<u8>> {
    capture(
        Command::new("debugfs")
            .arg("-R")
            .arg(format!("cat {}", quote(Path::new(path))?))
            .arg(image),
        maximum,
    )
}

fn inventory(image: &Path) -> Result<BTreeMap<String, Entry>> {
    fn visit(
        image: &Path,
        path: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        entries: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        if entries.len() >= MAX_ENTRIES || path.matches('/').count() > 32 {
            return Err("ext4 inventory exceeds integration limits".into());
        }
        let absolute = format!("/{path}");
        let stat = debug(image, &format!("stat {}", quote(Path::new(&absolute))?))?;
        let mtime = stat
            .lines()
            .find_map(|line| line.trim().strip_prefix("mtime:"))
            .and_then(|line| line.split_whitespace().next())
            .ok_or("missing ext4 inode mtime")?
            .to_owned();
        let xattrs = debug(image, &format!("ea_list {}", quote(Path::new(&absolute))?))?;
        let (content, link) = match mode & 0o170000 {
            0o100000 => {
                let mut tmp = tempfile::tempfile()?;
                stream(
                    Command::new("debugfs")
                        .arg("-R")
                        .arg(format!("cat {}", quote(Path::new(&absolute))?))
                        .arg(image),
                    &mut tmp,
                    MAX_FILE,
                )?;
                // Hash through the open file's procfs identity without naming a user-controlled path.
                use std::os::fd::AsRawFd;
                let digest =
                    FileDigest::read(Path::new(&format!("/proc/self/fd/{}", tmp.as_raw_fd())))?;
                (Some(digest), None)
            }
            0o120000 => {
                let target = if let Some(target) = stat
                    .lines()
                    .find_map(|line| line.strip_prefix("Fast link dest: \""))
                {
                    target
                        .strip_suffix('"')
                        .ok_or("invalid ext4 symlink")?
                        .to_owned()
                } else {
                    String::from_utf8(read_file(image, &absolute, 4096)?)?
                };
                (None, Some(target))
            }
            0o040000 => (None, None),
            _ => return Err(format!("unsupported inode type in base image: {absolute}").into()),
        };
        if entries
            .insert(
                path.to_owned(),
                Entry {
                    mode,
                    uid,
                    gid,
                    mtime,
                    xattrs,
                    content,
                    link,
                },
            )
            .is_some()
        {
            return Err("duplicate ext4 inventory path".into());
        }
        if mode & 0o170000 == 0o040000 {
            let listing = debug(image, &format!("ls -l -p {}", quote(Path::new(&absolute))?))?;
            for line in listing.lines().filter(|s| !s.trim().is_empty()) {
                let fields: Vec<_> = line.split('/').collect();
                if fields.len() != 8 || !fields[0].is_empty() || !fields[7].is_empty() {
                    return Err(format!("unexpected debugfs directory listing: {line}").into());
                }
                if fields[1] == "0" || [".", ".."].contains(&fields[5]) {
                    continue;
                }
                if !normal_relative(fields[5]) || fields[5].contains('/') {
                    return Err(format!("unsafe ext4 directory entry: {:?}", fields[5]).into());
                }
                let child = if path.is_empty() {
                    fields[5].to_owned()
                } else {
                    format!("{path}/{}", fields[5])
                };
                visit(
                    image,
                    &child,
                    u32::from_str_radix(fields[2], 8)?,
                    fields[3].parse()?,
                    fields[4].parse()?,
                    entries,
                )?;
            }
        }
        Ok(())
    }
    let root_stat = debug(image, "stat /")?;
    let root_mode = root_stat
        .lines()
        .next()
        .and_then(|s| s.split_once("Mode:"))
        .and_then(|(_, v)| v.split_whitespace().next())
        .ok_or("missing root mode")?;
    let root_ids = root_stat
        .lines()
        .find(|s| s.starts_with("User:"))
        .ok_or("missing root ownership")?;
    let ids: Vec<_> = root_ids.split_whitespace().collect();
    let mut entries = BTreeMap::new();
    visit(
        image,
        "",
        0o040000 | u32::from_str_radix(root_mode, 8)?,
        ids.get(1).ok_or("missing root UID")?.parse()?,
        ids.get(3).ok_or("missing root GID")?.parse()?,
        &mut entries,
    )?;
    Ok(entries)
}

pub fn derive(
    source: &Path,
    destination: &Path,
    additions: &BTreeMap<String, FileDigest>,
    tree: &Path,
) -> Result<()> {
    if !is_ext4(source)? {
        return Err("expected a raw ext4 input image".into());
    }
    new_output(destination)?;
    eprintln!("Inventorying {}", source.display());
    let source_digest = FileDigest::read(source)?;
    let before = inventory(source)?;
    let mut dirs = BTreeSet::new();
    for (path, digest) in additions {
        if !normal_relative(path) || before.contains_key(path) {
            return Err(format!("GApps must not overwrite an existing base file: {path}").into());
        }
        if FileDigest::read(&tree.join(path))? != *digest {
            return Err("selected payload changed before assembly".into());
        }
        let mut parent = Path::new(path).parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            let name = path.to_str().ok_or("invalid selected path")?;
            if let Some(entry) = before.get(name) {
                if entry.mode & 0o170000 != 0o040000 {
                    return Err(
                        format!("GApps path crosses a non-directory base inode: {name}").into(),
                    );
                }
            } else {
                dirs.insert(name.to_owned());
            }
            parent = path.parent();
        }
    }
    // Refuse any base already carrying the named Google apps, even at another path.
    if before.keys().any(|p| {
        ["/GmsCore/", "/Phonesky/", "/GoogleServicesFramework/"]
            .iter()
            .any(|name| p.contains(name))
    }) {
        return Err("base already contains Google core applications".into());
    }
    run(Command::new("cp")
        .args(["--reflink=auto", "--sparse=always", "--"])
        .arg(source)
        .arg(destination))?;
    // Grow the copied filesystem before adding APKs; preserve all original inodes.
    check(destination)?;
    let extra: u64 = additions.values().map(|d| d.size).sum();
    let grown = (source_digest.size + extra + 64 * 1024 * 1024).div_ceil(4096) * 4096;
    fs::OpenOptions::new()
        .write(true)
        .open(destination)?
        .set_len(grown)?;
    run(Command::new("resize2fs")
        .env("E2FSPROGS_FAKE_TIME", EPOCH.to_string())
        .arg(destination))?;
    let work = tempfile::tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let label = work.path().join("selinux");
    fs::write(&label, b"u:object_r:system_file:s0\0")?;
    let mut commands = String::new();
    let mut new_paths = BTreeMap::new();
    for directory in &dirs {
        commands.push_str(&format!("mkdir /{directory}\n"));
        new_paths.insert(directory.clone(), 0o040755);
    }
    for path in additions.keys() {
        commands.push_str(&format!("write {} /{path}\n", quote(&tree.join(path))?));
        new_paths.insert(path.clone(), 0o100644);
    }
    for (path, mode) in &new_paths {
        for (field, value) in [
            ("uid", "0".to_owned()),
            ("gid", "0".to_owned()),
            ("mode", format!("0{mode:o}")),
            ("atime", format!("0x{EPOCH:x}")),
            ("mtime", format!("0x{EPOCH:x}")),
            ("ctime", format!("0x{EPOCH:x}")),
            ("crtime", format!("0x{EPOCH:x}")),
            ("atime_extra", "0".to_owned()),
            ("mtime_extra", "0".to_owned()),
            ("ctime_extra", "0".to_owned()),
            ("crtime_extra", "0".to_owned()),
        ] {
            commands.push_str(&format!("set_inode_field /{path} {field} {value}\n"));
        }
        commands.push_str(&format!(
            "ea_set -f {} /{path} security.selinux\n",
            quote(&label)?
        ));
    }
    // Directory updates must not change base mtimes used by Android's parser cache.
    for (path, entry) in &before {
        if entry.mode & 0o170000 == 0o040000 {
            let (seconds, extra) = entry
                .mtime
                .split_once(':')
                .ok_or("unsupported ext4 timestamp")?;
            commands.push_str(&format!("set_inode_field /{path} mtime {seconds}\nset_inode_field /{path} mtime_extra 0x{extra}\n"));
        }
    }
    let script = work.path().join("additions.debugfs");
    fs::write(&script, commands)?;
    run(Command::new("debugfs")
        .env("E2FSPROGS_FAKE_TIME", EPOCH.to_string())
        .arg("-w")
        .arg("-f")
        .arg(&script)
        .arg(destination))?;
    check(destination)?;
    run(Command::new("resize2fs")
        .env("E2FSPROGS_FAKE_TIME", EPOCH.to_string())
        .arg("-M")
        .arg(destination))?;
    check(destination)?;
    let after = inventory(destination)?;
    for (path, entry) in &before {
        if after.get(path) != Some(entry) {
            return Err(format!(
                "base file content or metadata changed: {path}; before={entry:?}; after={:?}",
                after.get(path)
            )
            .into());
        }
    }
    if after.len() != before.len() + new_paths.len() {
        return Err("derived image has unexpected extra or missing entries".into());
    }
    for (path, mode) in new_paths {
        let entry = after.get(&path).ok_or("missing added inode")?;
        if entry.mode != mode
            || entry.uid != 0
            || entry.gid != 0
            || entry.mtime != format!("0x{EPOCH:08x}:00000000")
            || !entry.xattrs.contains("security.selinux")
            || !entry.xattrs.contains("u:object_r:system_file:s0")
            || entry.content.as_ref() != additions.get(&path)
        {
            return Err(format!("wrong content or metadata for added inode: {path}").into());
        }
    }
    if FileDigest::read(source)? != source_digest {
        return Err("base image changed during derivation".into());
    }
    eprintln!(
        "Verified {} retained entries and {} additions in {}",
        before.len(),
        additions.len(),
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_base_metadata_and_rejects_overwrite_and_symlink_parents() {
        let work = tempfile::tempdir().unwrap();
        let image = work.path().join("base.img");
        fs::File::create(&image)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        run(Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-F"])
            .arg(&image))
        .unwrap();
        let source = work.path().join("source");
        fs::write(&source, "retained bytes").unwrap();
        let script = work.path().join("fixture.debugfs");
        fs::write(&script, format!("mkdir /priv-app\nwrite {} /retained\nset_inode_field /retained uid 1000\nset_inode_field /retained gid 2000\nset_inode_field /retained mode 0100640\nsymlink /link /priv-app\n", quote(&source).unwrap())).unwrap();
        run(Command::new("debugfs")
            .arg("-w")
            .arg("-f")
            .arg(script)
            .arg(&image))
        .unwrap();
        let tree = work.path().join("tree");
        write(&tree.join("priv-app/GmsCore/GmsCore.apk"), "new apk").unwrap();
        let additions: BTreeMap<_, _> = [(
            "priv-app/GmsCore/GmsCore.apk".into(),
            FileDigest::read(&tree.join("priv-app/GmsCore/GmsCore.apk")).unwrap(),
        )]
        .into();
        derive(&image, &work.path().join("derived.img"), &additions, &tree).unwrap();
        let wrong: BTreeMap<_, _> = [(
            "retained".into(),
            additions.values().next().unwrap().clone(),
        )]
        .into();
        assert!(derive(&image, &work.path().join("overwrite.img"), &wrong, &tree).is_err());
        write(&tree.join("link/new.apk"), "new apk").unwrap();
        let wrong: BTreeMap<_, _> = [(
            "link/new.apk".into(),
            FileDigest::read(&tree.join("link/new.apk")).unwrap(),
        )]
        .into();
        assert!(derive(&image, &work.path().join("symlink.img"), &wrong, &tree).is_err());
    }
}
