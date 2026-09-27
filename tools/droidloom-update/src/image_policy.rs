//! Derive the pinned images whose device-facing policy Droidloom changes: the
//! desktop system_ext without legacy vendor compatibility APEXes, and the
//! tablet product whose build characteristic declares its device class.
use crate::util::*;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

const LEGACY_VNDK: [&str; 4] = [
    "apex/com.android.vndk.v31.apex",
    "apex/com.android.vndk.v32.apex",
    "apex/com.android.vndk.v33.apex",
    "apex/com.android.vndk.v34.apex",
];

fn desktop_vendor(properties: &str) -> Result<()> {
    let properties: BTreeMap<_, _> = properties
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                None
            } else {
                line.split_once('=')
            }
        })
        .collect();
    // The vendor device selects its CPU list; cross products must not smuggle
    // an x86_64 ABI declaration into an ARM64 cell or vice versa.
    let abilist = match properties.get("ro.product.vendor.device") {
        Some(&"droidloom_x86_64") => "x86_64",
        Some(&"droidloom_arm64") | Some(&"droidloom_sheng") => "arm64-v8a",
        _ => {
            return fail(
                "legacy VNDK removal requires a Droidloom vendor without a VNDK selection",
            );
        }
    };
    if properties.get("ro.vendor.build.version.sdk") != Some(&"37")
        || !properties
            .get("ro.vendor.product.cpu.abilist")
            .is_some_and(|abilist_value| {
                abilist_value.split(',').any(|entry| entry.trim() == abilist)
            })
        || ["ro.vndk.version", "ro.product.vndk.version"]
            .iter()
            .any(|key| properties.get(key).is_some_and(|value| !value.is_empty()))
    {
        return fail(
            "legacy VNDK removal requires the Android 17 Droidloom vendor without a VNDK selection",
        );
    }
    Ok(())
}

pub fn stage(source: &Path, destination: &Path, vendor_properties: &Path) -> Result<()> {
    desktop_vendor(&fs::read_to_string(vendor_properties)?)?;
    fs::create_dir_all(destination.parent().ok_or("image has no parent")?)?;
    // A single fakeroot session preserves otherwise privileged ownership and
    // security xattrs across extraction, mkfs, and independent re-extraction.
    // Cargo can replace this updater while it builds the host tools. Read the
    // running executable directly, even when its old pathname was unlinked.
    let worker = tempfile::Builder::new()
        .prefix("image-worker-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let executable = worker.path().join("droidloom-update");
    fs::copy("/proc/self/exe", &executable)?;
    crate::util::mode(&executable, 0o755)?;
    run(Command::new("fakeroot")
        .arg("--")
        .arg(&executable)
        .arg("prune-desktop-image")
        .arg("--image")
        .arg(source)
        .arg("--destination")
        .arg(destination)
        .arg("--vendor-properties")
        .arg(vendor_properties))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    uid: u32,
    gid: u32,
    mode: u32,
    modified: (i64, i64),
    content: String,
    attributes: String,
}

pub(crate) fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    fn visit(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
        let path = root.join(relative);
        let meta = fs::symlink_metadata(&path)?;
        let content = if meta.is_file() {
            hash(&path)?
        } else if meta.file_type().is_symlink() {
            fs::read_link(&path)?.to_string_lossy().into_owned()
        } else if meta.is_dir() {
            String::new()
        } else {
            return fail(format!(
                "unexpected object in system_ext: {}",
                relative.display()
            ));
        };
        let attributes = output(
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
            relative.to_owned(),
            Entry {
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.mode(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                content,
                attributes,
            },
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(root, &relative.join(entry?.file_name()), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(root, Path::new(""), &mut entries)?;
    Ok(entries)
}

pub(crate) fn extract(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    run(Command::new("fsck.erofs")
        .arg("--preserve")
        .arg("--xattrs")
        .arg(format!("--extract={}", destination.display()))
        .arg(image))
}

pub fn prune(image: &Path, destination: &Path, vendor_properties: &Path) -> Result<()> {
    if std::env::var_os("FAKEROOTKEY").is_none() {
        return fail("image derivation must run inside fakeroot to preserve Android metadata");
    }
    desktop_vendor(&fs::read_to_string(vendor_properties)?)?;
    if destination.exists() {
        return fail("derived image destination already exists");
    }
    let work = tempfile::Builder::new()
        .prefix("vndk-prune-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let tree = work.path().join("tree");
    extract(image, &tree)?;
    let mut expected = inventory(&tree)?;
    for relative in LEGACY_VNDK {
        if !fs::symlink_metadata(tree.join(relative))?.is_file() {
            return fail(format!("expected a regular legacy VNDK APEX: {relative}"));
        }
        fs::remove_file(tree.join(relative))?;
        expected
            .remove(Path::new(relative))
            .ok_or("missing VNDK inventory entry")?;
    }
    // Removing directory entries changes the staging directory's mtime only.
    let apex = expected
        .get(Path::new("apex"))
        .ok_or("missing apex directory")?;
    fs::File::open(tree.join("apex"))?.set_modified(
        std::time::UNIX_EPOCH
            + std::time::Duration::new(apex.modified.0.try_into()?, apex.modified.1.try_into()?),
    )?;
    let rebuilt = rebake_verified(work.path(), image, &tree, &expected)?;
    eprintln!(
        "Removed four legacy VNDK APEXes; system_ext: {} -> {} bytes; retained contents, ownership, modes, timestamps and xattrs verified",
        fs::metadata(image)?.len(),
        fs::metadata(&rebuilt)?.len()
    );
    fs::rename(rebuilt, destination)?;
    Ok(())
}

/// Declare the tablet device class in the product image of the tablet product.
///
/// Platforms and applications read this characteristic to choose between phone
/// and tablet presentation. The tablet product presents one large landscape
/// display, so its image declares the class that display establishes, while the
/// products that present a phone-sized display keep the class the pinned base
/// image states. Only that one line changes; every other property, file and
/// metadata entry keeps its pinned value.
pub fn declare_tablet(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination.parent().ok_or("image has no parent")?)?;
    // A single fakeroot session preserves otherwise privileged ownership and
    // security xattrs across extraction, mkfs, and independent re-extraction.
    let worker = tempfile::Builder::new()
        .prefix("tablet-declaration-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let executable = worker.path().join("droidloom-update");
    fs::copy("/proc/self/exe", &executable)?;
    mode(&executable, 0o755)?;
    run(Command::new("fakeroot")
        .arg("--")
        .arg(&executable)
        .arg("declare-tablet-product-image")
        .arg("--image")
        .arg(image)
        .arg("--destination")
        .arg(destination))
}

pub fn declare_tablet_product_image(image: &Path, destination: &Path) -> Result<()> {
    if std::env::var_os("FAKEROOTKEY").is_none() {
        return fail("tablet declaration must run inside fakeroot to preserve Android metadata");
    }
    if destination.exists() {
        return fail("declared product image destination already exists");
    }
    let work = tempfile::Builder::new()
        .prefix("tablet-declaration-")
        .tempdir_in(destination.parent().ok_or("image has no parent")?)?;
    let tree = work.path().join("tree");
    extract(image, &tree)?;
    // A product image is an EROFS filesystem for /product, so its root is the
    // partition root: the properties file sits directly under etc.
    let properties = tree.join("etc/build.prop");
    // The pinned base image declares the phone class once, without a prefix.
    const PHONE: &str = "ro.build.characteristics=default";
    const TABLET: &str = "ro.build.characteristics=tablet";
    let contents = fs::read_to_string(&properties)?;
    let declared = match contents.matches(PHONE).count() {
        1 => contents.replace(PHONE, TABLET),
        0 => {
            return fail(format!(
                "tablet product image declares no phone characteristic to replace: {}",
                properties.display()
            ));
        }
        count => {
            return fail(format!(
                "tablet product image declares the phone characteristic {count} times"
            ));
        }
    };
    // Rewriting the file in place keeps its owner, mode and security label; the
    // original modification time is restored so a derived image stays
    // reproducible from its base.
    let modified = fs::metadata(&properties)?.modified()?;
    fs::write(&properties, declared)?;
    fs::File::open(&properties)?.set_modified(modified)?;
    let expected = inventory(&tree)?;
    let rebuilt = rebake_verified(work.path(), image, &tree, &expected)?;
    eprintln!(
        "Declared the tablet device class; product: {} -> {} bytes; retained contents, ownership, modes, timestamps and xattrs verified",
        fs::metadata(image)?.len(),
        fs::metadata(&rebuilt)?.len()
    );
    fs::rename(rebuilt, destination)?;
    Ok(())
}

/// The EROFS identity a derived image has to keep. Android mounts these images
/// by path, but every other reader of the image sees the filesystem UUID, so a
/// derivation keeps the value its source states instead of inventing one.
fn filesystem_uuid(image: &Path) -> Result<String> {
    let report = output(Command::new("dump.erofs").arg("-s").arg(image))?;
    match report
        .lines()
        .find_map(|line| line.trim().strip_prefix("Filesystem UUID:"))
    {
        Some(uuid) => Ok(uuid.trim().to_owned()),
        None => fail(format!(
            "no EROFS filesystem identity in {}; erofs-utils could not read it",
            image.display()
        )),
    }
}

/// Rebuild an extracted tree as EROFS with the source image's identity, then
/// prove that every retained entry survived the round trip byte for byte.
fn rebake_verified(
    work: &Path,
    source: &Path,
    tree: &Path,
    expected: &BTreeMap<PathBuf, Entry>,
) -> Result<PathBuf> {
    let uuid = filesystem_uuid(source)?;
    let rebuilt = work.join("rebuilt.img");
    run(Command::new("mkfs.erofs")
        // makepkg exports SOURCE_DATE_EPOCH, which otherwise silently clamps
        // newer mtimes despite --preserve-mtime. Preserve the staged metadata
        // exactly; the filesystem creation timestamp is explicitly pinned below.
        .env_remove("SOURCE_DATE_EPOCH")
        .args([
            "-zlz4hc,level=9",
            "--workers=2",
            "-T1230768000",
            "--mkfs-time",
            "--preserve-mtime",
        ])
        .arg(format!("-U{uuid}"))
        .arg(&rebuilt)
        .arg(tree))?;
    let checked = work.join("checked");
    extract(&rebuilt, &checked)?;
    let actual = inventory(&checked)?;
    if &actual != expected {
        let changed: Vec<_> = expected
            .keys()
            .chain(actual.keys())
            .filter(|path| expected.get(*path) != actual.get(*path))
            .collect();
        for path in changed.iter().take(4) {
            eprintln!(
                "{path:?}: expected {:?}; actual {:?}",
                expected.get(*path),
                actual.get(*path)
            );
        }
        return fail(format!(
            "derived image changed retained file contents or metadata: {changed:?}"
        ));
    }
    if filesystem_uuid(&rebuilt)? != uuid {
        return fail("derived image did not keep its source filesystem identity");
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROPERTIES: &str = "ro.product.vendor.device=droidloom_x86_64\nro.vendor.build.version.sdk=37\nro.vendor.product.cpu.abilist=x86_64\n";
    #[test]
    fn rejects_legacy_vendor_and_other_products() {
        desktop_vendor(PROPERTIES).unwrap();
        for device in ["droidloom_arm64", "droidloom_sheng"] {
            let properties = PROPERTIES
                .replace("droidloom_x86_64", device)
                .replace("abilist=x86_64", "abilist=arm64-v8a");
            desktop_vendor(&properties).unwrap();
            // A foreign ABI declaration never belongs to this vendor device.
            assert!(desktop_vendor(&format!("{properties}ro.vendor.product.cpu.abilist=x86_64\n")).is_err());
        }
        for properties in [
            PROPERTIES.replace("sdk=37", "sdk=34"),
            PROPERTIES.replace("droidloom_x86_64", "phone"),
            format!("{PROPERTIES}ro.vndk.version=34\n"),
            format!("{PROPERTIES}ro.product.vndk.version=31\n"),
        ] {
            assert!(desktop_vendor(&properties).is_err());
        }
    }
    #[test]
    fn tablet_declaration_changes_only_the_device_class() {
        if std::env::var_os("DROIDLOOM_TABLET_DECLARATION_TEST").is_none() {
            run(Command::new("fakeroot")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "image_policy::tests::tablet_declaration_changes_only_the_device_class",
                    "--nocapture",
                ])
                .env("DROIDLOOM_TABLET_DECLARATION_TEST", "1"))
            .unwrap();
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("input");
        fs::create_dir_all(root.join("etc")).unwrap();
        let properties = root.join("etc/build.prop");
        fs::write(
            &properties,
            "ro.product.model=Cuttlefish arm64 phone\nro.build.characteristics=default\nro.vendor.build.characteristics=default\n",
        )
        .unwrap();
        let original = work.path().join("original.img");
        run(Command::new("mkfs.erofs")
            .args(["-zlz4hc", "--workers=2"])
            .arg(&original)
            .arg(&root))
        .unwrap();
        let destination = work.path().join("product.img");
        // This test already runs inside the fakeroot session `declare_tablet`
        // would otherwise start, and a nested session is refused, so it drives
        // the declaration itself. The wrapper around it only copies this
        // executable into the work directory and starts that one session.
        declare_tablet_product_image(&original, &destination).unwrap();
        let check = work.path().join("verify");
        extract(&destination, &check).unwrap();
        // The declared class changes; the product identity and every other
        // characteristic keep the value the base image states.
        assert_eq!(
            fs::read_to_string(check.join("etc/build.prop")).unwrap(),
            "ro.product.model=Cuttlefish arm64 phone\nro.build.characteristics=tablet\nro.vendor.build.characteristics=default\n"
        );
        assert!(
            fs::metadata(&destination).unwrap().len() > 0,
            "derived product image is empty"
        );
        // The derivation re-states the source filesystem identity rather than
        // inventing one, so every reader of the image keeps seeing the same
        // filesystem.
        assert_eq!(
            filesystem_uuid(&destination).unwrap(),
            filesystem_uuid(&original).unwrap(),
        );
        // One derivation per base image: reusing the destination is refused.
        assert!(declare_tablet_product_image(&original, &destination).is_err());
        // An image without the phone class can not declare the tablet class.
        fs::write(&properties, "ro.product.model=Cuttlefish arm64 phone\n").unwrap();
        let absent = work.path().join("absent.img");
        run(Command::new("mkfs.erofs")
            .args(["-zlz4hc", "--workers=2"])
            .arg(&absent)
            .arg(&root))
        .unwrap();
        assert!(
            declare_tablet_product_image(&absent, &work.path().join("absent-out.img")).is_err()
        );
    }
    #[test]
    fn repack_preserves_android_metadata_and_adbd() {
        if std::env::var_os("DROIDLOOM_IMAGE_POLICY_TEST").is_none() {
            run(Command::new("fakeroot")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "image_policy::tests::repack_preserves_android_metadata_and_adbd",
                    "--nocapture",
                ])
                .env("DROIDLOOM_IMAGE_POLICY_TEST", "1"))
            .unwrap();
            return;
        }
        use std::os::unix::fs::{PermissionsExt, symlink};
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("input");
        fs::create_dir_all(root.join("apex")).unwrap();
        for file in LEGACY_VNDK
            .into_iter()
            .chain(["apex/com.android.adbd.apex"])
        {
            fs::write(root.join(file), vec![42; 8192]).unwrap();
        }
        let adbd = root.join("apex/com.android.adbd.apex");
        run(Command::new("chown").arg("1000:2000").arg(&adbd)).unwrap();
        fs::set_permissions(&adbd, fs::Permissions::from_mode(0o640)).unwrap();
        run(Command::new("setfattr")
            .args(["-n", "security.selinux", "-v", "u:object_r:system_file:s0"])
            .arg(&adbd))
        .unwrap();
        symlink("com.android.adbd.apex", root.join("apex/retained-link")).unwrap();
        let properties = work.path().join("vendor.prop");
        fs::write(&properties, PROPERTIES).unwrap();
        let original = work.path().join("original.img");
        run(Command::new("mkfs.erofs")
            .args(["-zlz4hc", "--workers=2"])
            .arg(&original)
            .arg(&root))
        .unwrap();
        let destination = work.path().join("derived.img");
        prune(&original, &destination, &properties).unwrap();
        let check = work.path().join("verify");
        extract(&destination, &check).unwrap();
        let retained = fs::metadata(check.join("apex/com.android.adbd.apex")).unwrap();
        assert_eq!(
            (retained.uid(), retained.gid(), retained.mode() & 0o777),
            (1000, 2000, 0o640)
        );
        assert!(LEGACY_VNDK.iter().all(|file| !check.join(file).exists()));
        assert!(prune(&original, &destination, &properties).is_err());
    }
}
