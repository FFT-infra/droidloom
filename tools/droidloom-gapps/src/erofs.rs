//! Rootless additive EROFS derivation with retained content and metadata checks.
use crate::util::*;
use droidloom_contracts::gapps::FileDigest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, UNIX_EPOCH},
};

const EPOCH: u64 = 1_230_768_000;

pub fn is_erofs(image: &Path) -> Result<bool> {
    let mut file = fs::File::open(image)?;
    file.seek(SeekFrom::Start(1024))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    Ok(magic == [0xe2, 0xe1, 0xf5, 0xe0])
}

#[derive(Debug, Eq, PartialEq)]
struct Entry {
    uid: u32,
    gid: u32,
    mode: u32,
    modified: (i64, i64),
    content: Option<FileDigest>,
    link: Option<PathBuf>,
    attributes: String,
}

fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    fn visit(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
        if entries.len() >= 32768 || relative.components().count() > 32 {
            return Err("EROFS inventory exceeds integration limits".into());
        }
        let path = root.join(relative);
        let meta = fs::symlink_metadata(&path)?;
        let (content, link) = if meta.is_file() {
            (Some(FileDigest::read(&path)?), None)
        } else if meta.file_type().is_symlink() {
            (None, Some(fs::read_link(&path)?))
        } else if meta.is_dir() {
            (None, None)
        } else {
            return Err("unsupported EROFS inode type".into());
        };
        let attributes = text(
            Command::new("getfattr")
                .args([
                    "--absolute-names",
                    "--dump",
                    "--no-dereference",
                    "--encoding=hex",
                    "--match=-",
                ])
                .arg(&path),
        )?
        .lines()
        .filter(|line| !line.starts_with("# file:"))
        .collect::<Vec<_>>()
        .join("\n");
        entries.insert(
            relative.into(),
            Entry {
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.mode(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                content,
                link,
                attributes,
            },
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                let child = relative.join(entry?.file_name());
                if !normal_relative(child.to_str().ok_or("non-UTF8 EROFS path")?) {
                    return Err("unsupported EROFS path".into());
                }
                visit(root, &child, entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(root, Path::new(""), &mut entries)?;
    Ok(entries)
}

fn extract(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    run(Command::new("fsck.erofs")
        .args(["--preserve", "--xattrs"])
        .arg(format!("--extract={}", destination.display()))
        .arg(image))
}

pub fn derive(
    source: &Path,
    destination: &Path,
    additions: &BTreeMap<String, FileDigest>,
    tree: &Path,
) -> Result<()> {
    new_output(destination)?;
    let work = tempfile::tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let executable = work.path().join("worker");
    fs::copy("/proc/self/exe", &executable)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let manifest = work.path().join("additions.json");
    fs::write(&manifest, serde_json::to_vec(additions)?)?;
    run(Command::new("fakeroot")
        .arg("--")
        .arg(executable)
        .arg("derive-erofs")
        .arg(source)
        .arg(destination)
        .arg(manifest)
        .arg(tree))
}

pub fn derive_worker(
    source: &Path,
    destination: &Path,
    additions: &BTreeMap<String, FileDigest>,
    tree: &Path,
) -> Result<()> {
    if std::env::var_os("FAKEROOTKEY").is_none() {
        return Err("EROFS worker requires fakeroot to preserve Android metadata".into());
    }
    if !is_erofs(source)? {
        return Err("expected an EROFS input".into());
    }
    new_output(destination)?;
    let digest = FileDigest::read(source)?;
    let work = tempfile::tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let extracted = work.path().join("tree");
    extract(source, &extracted)?;
    let before = inventory(&extracted)?;
    if before.keys().any(|p| {
        p.components().any(|c| {
            ["GmsCore", "Phonesky", "GoogleServicesFramework"]
                .iter()
                .any(|name| c.as_os_str() == *name)
        })
    }) {
        return Err("base already contains Google core applications".into());
    }
    let mut directories = BTreeSet::new();
    for (path, expected) in additions {
        if !normal_relative(path) || before.contains_key(Path::new(path)) {
            return Err(format!("GApps must not overwrite an existing base file: {path}").into());
        }
        if FileDigest::read(&tree.join(path))? != *expected {
            return Err("selected payload changed before assembly".into());
        }
        let mut parent = Path::new(path).parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            if let Some(entry) = before.get(path) {
                if entry.mode & 0o170000 != 0o040000 {
                    return Err("GApps path crosses a non-directory base inode".into());
                }
            } else {
                directories.insert(path.to_owned());
            }
            parent = path.parent();
        }
    }
    for path in &directories {
        fs::create_dir(extracted.join(path))?;
    }
    for path in additions.keys() {
        fs::copy(tree.join(path), extracted.join(path))?;
    }
    for (path, mode) in directories
        .iter()
        .map(|p| (p.as_path(), 0o755))
        .chain(additions.keys().map(|p| (Path::new(p), 0o644)))
    {
        let path = extracted.join(path);
        run(Command::new("chown").arg("0:0").arg(&path))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        run(Command::new("setfattr")
            .args([
                "-n",
                "security.selinux",
                "-v",
                "0x753a6f626a6563745f723a73797374656d5f66696c653a733000",
            ])
            .arg(&path))?;
        fs::File::open(path)?.set_modified(UNIX_EPOCH + Duration::from_secs(EPOCH))?;
    }
    for (path, entry) in &before {
        if entry.mode & 0o170000 == 0o040000 {
            fs::File::open(extracted.join(path))?.set_modified(
                UNIX_EPOCH
                    + Duration::new(entry.modified.0.try_into()?, entry.modified.1.try_into()?),
            )?;
        }
    }
    let expected = inventory(&extracted)?;
    for (path, entry) in &before {
        if expected.get(path) != Some(entry) {
            return Err(format!("base metadata changed while adding GApps: {path:?}").into());
        }
    }
    if expected.len() != before.len() + directories.len() + additions.len() {
        return Err("unexpected added EROFS entries".into());
    }
    for (path, digest) in additions {
        let entry = expected.get(Path::new(path)).ok_or("missing addition")?;
        if entry.content.as_ref() != Some(digest)
            || entry.uid != 0
            || entry.gid != 0
            || entry.mode != 0o100644
        {
            return Err("wrong added APK content or metadata".into());
        }
    }
    let rebuilt = work.path().join("rebuilt.img");
    run(Command::new("mkfs.erofs")
        .args([
            "-zlz4hc,level=9",
            "--workers=1",
            "-T1230768000",
            "--mkfs-time",
            "--preserve-mtime",
        ])
        .arg(&rebuilt)
        .arg(&extracted))?;
    let checked = work.path().join("checked");
    extract(&rebuilt, &checked)?;
    let actual = inventory(&checked)?;
    if actual != expected {
        let changed: Vec<_> = expected
            .keys()
            .chain(actual.keys())
            .filter(|path| expected.get(*path) != actual.get(*path))
            .take(8)
            .collect();
        return Err(format!("EROFS rebuild changed file contents or metadata: {changed:?}").into());
    }
    if FileDigest::read(source)? != digest {
        return Err("base image changed during derivation".into());
    }
    fs::rename(rebuilt, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn additive_image_preserves_metadata_and_rejects_collisions() {
        if std::env::var_os("DROIDLOOM_GAPPS_EROFS_TEST").is_none() {
            run(Command::new("fakeroot")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "erofs::tests::additive_image_preserves_metadata_and_rejects_collisions",
                    "--nocapture",
                ])
                .env("DROIDLOOM_GAPPS_EROFS_TEST", "1"))
            .unwrap();
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("input");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("retained"), b"original").unwrap();
        run(Command::new("chown")
            .arg("1000:2000")
            .arg(root.join("retained")))
        .unwrap();
        fs::set_permissions(root.join("retained"), fs::Permissions::from_mode(0o640)).unwrap();
        run(Command::new("setfattr")
            .args(["-n", "security.selinux", "-v", "u:object_r:system_file:s0"])
            .arg(root.join("retained")))
        .unwrap();
        std::os::unix::fs::symlink("retained", root.join("link")).unwrap();
        let source = work.path().join("base.img");
        run(Command::new("mkfs.erofs")
            .arg("--workers=1")
            .arg(&source)
            .arg(&root))
        .unwrap();
        let payload = work.path().join("payload");
        fs::create_dir_all(payload.join("priv-app/Phonesky")).unwrap();
        fs::write(payload.join("priv-app/Phonesky/Phonesky.apk"), b"apk").unwrap();
        let additions = [(
            "priv-app/Phonesky/Phonesky.apk".into(),
            FileDigest::read(&payload.join("priv-app/Phonesky/Phonesky.apk")).unwrap(),
        )]
        .into();
        derive_worker(&source, &work.path().join("good.img"), &additions, &payload).unwrap();
        let collision = [(
            "retained".into(),
            FileDigest::read(&root.join("retained")).unwrap(),
        )]
        .into();
        assert!(
            derive_worker(
                &source,
                &work.path().join("collision.img"),
                &collision,
                &root
            )
            .is_err()
        );
        fs::create_dir(payload.join("link")).unwrap();
        fs::write(payload.join("link/child.apk"), b"apk").unwrap();
        let crossing = [(
            "link/child.apk".into(),
            FileDigest::read(&payload.join("link/child.apk")).unwrap(),
        )]
        .into();
        assert!(
            derive_worker(
                &source,
                &work.path().join("crossing.img"),
                &crossing,
                &payload
            )
            .unwrap_err()
            .to_string()
            .contains("crosses a non-directory base inode")
        );
        assert!(!work.path().join("collision.img").exists());
        assert!(!work.path().join("crossing.img").exists());
    }
}
