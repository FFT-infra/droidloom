//! Presenter-only derivatives retain every unchanged component's verified identity.

use crate::{bundle::{self, BuildProvenance, BundleKind, Manifest}, util::*};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::Command};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresenterBuildRecord {
    pub schema: u32,
    pub source_commit: String,
    pub source_dirty: bool,
    pub cargo_lock_sha256: String,
    pub source_lock_sha256: String,
    pub target: String,
    pub toolchain: String,
    pub build_profile: String,
    pub build_flags: Vec<String>,
    pub presenter_sha256: String,
}

impl PresenterBuildRecord {
    pub fn write_after_build(
        repo: &Path,
        provenance: &BuildProvenance,
        presenter: &Path,
        path: &Path,
        flags: Vec<String>,
    ) -> Result<()> {
        provenance.check_unchanged(repo)?;
        let record = Self {
            schema: 1,
            source_commit: provenance.source_commit.clone(),
            source_dirty: false,
            cargo_lock_sha256: provenance.cargo_lock_sha256.clone(),
            source_lock_sha256: provenance.source_lock_sha256.clone(),
            target: provenance.target.clone(),
            toolchain: provenance.toolchain.clone(),
            build_profile: "release".into(),
            build_flags: flags,
            presenter_sha256: hash(presenter)?,
        };
        durable_write(path, serde_json::to_vec_pretty(&record)?)
    }

    fn validate(&self, repo: &Path, presenter: &Path, base: &Manifest) -> Result<()> {
        let current = BuildProvenance::capture(repo, &base.architecture)?;
        if self.schema != 1 || self.source_dirty || self.source_commit != current.source_commit
            || self.cargo_lock_sha256 != current.cargo_lock_sha256
            || self.source_lock_sha256 != current.source_lock_sha256
            || self.target != current.target || self.toolchain.is_empty()
            || self.toolchain.len() > 1024 || self.toolchain.chars().any(char::is_control)
            || self.build_profile != "release" || !bundle::valid_digest(&self.presenter_sha256)
            || self.build_flags.len() > 128
            || self.build_flags.iter().any(|f| f.len() > 4096 || f.chars().any(char::is_control))
            || !self.build_flags.iter().any(|f| f == "--locked")
            || !self.build_flags.iter().any(|f| f == "--release")
        {
            return fail("presenter build record does not match the clean source and build locks");
        }
        let metadata = fs::symlink_metadata(presenter)?;
        if !metadata.is_file() || metadata.mode() & 0o7000 != 0 || metadata.mode() & 0o111 == 0
            || hash(presenter)? != self.presenter_sha256
            || bundle::executable_architecture(presenter)? != base.architecture
        {
            return fail("presenter bytes, executable mode or architecture disagree with build record");
        }
        let previous = base.components.iter().find(|c| c.name == "presenter")
            .ok_or("base has no presenter provenance")?;
        if previous.cargo_lock_sha256.as_deref() != Some(self.cargo_lock_sha256.as_str())
            || previous.source_lock_sha256.as_deref() != Some(self.source_lock_sha256.as_str())
            || previous.target != self.target
        {
            return fail("build locks or target changed; use a complete bundle build");
        }
        check_source_changes(repo, &previous.source_commit, &self.source_commit)
    }
}

fn allowed_source_path(path: &str) -> bool {
    bundle::valid_relative_path(path)
        && (path.starts_with("graphics/droidloom-wayland/src/")
            || path.starts_with("graphics/droidloom-wayland/tests/")
            || (path.starts_with("docs/") && path.ends_with(".md"))
            || matches!(path, "README.md" | "AGENTS.md" | "CHANGELOG.md"))
}

fn check_source_changes(repo: &Path, base: &str, current: &str) -> Result<()> {
    let ancestor = Command::new("git").current_dir(repo)
        .args(["merge-base", "--is-ancestor", base, current]).status()?;
    if !ancestor.success() { return fail("presenter source does not descend from the verified base"); }
    let changes = Command::new("git").current_dir(repo)
        .args(["diff", "--no-renames", "--name-only", "-z", base, current, "--"]).output()?;
    if !changes.status.success() { return fail("cannot compare presenter source revisions"); }
    for path in changes.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(path)?;
        if !allowed_source_path(path) {
            return fail(format!("presenter-only update cannot include {path}; build a complete bundle"));
        }
    }
    Ok(())
}

fn absolute_existing(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() { return fail("derivative inputs must be absolute paths"); }
    Ok(path.canonicalize()?)
}

fn output_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
        return fail("derivative output must be a canonical absolute path");
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return fail("derivative output already exists; refusing to overwrite it"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = path.parent().ok_or("derivative output has no parent")?.canonicalize()?;
    if !parent.is_dir() { return fail("derivative output parent is not a directory"); }
    Ok(parent.join(path.file_name().ok_or("derivative output has no name")?))
}

fn copy_base(base: &Path, stage: &Path, manifest: &Manifest) -> Result<()> {
    // The manifest's original hashes remain authoritative after the copy.
    let directory = open_directory(base)?;
    for artifact in &manifest.files {
        copy_beneath(&directory, Path::new(&artifact.path), &stage.join(&artifact.path))?;
    }
    Ok(())
}

pub fn run(repo: &Path, base: &Path, presenter: &Path, record: &Path, destination: &Path) -> Result<String> {
    let destination = output_path(destination)?;
    let base = absolute_existing(base)?;
    // Do not silently turn a symlink into an accepted presenter input.
    if !fs::symlink_metadata(presenter)?.is_file() { return fail("presenter input must be a regular file"); }
    let presenter = absolute_existing(presenter)?;
    let record_path = absolute_existing(record)?;
    let manifest = bundle::verify(&base)?;
    let record: PresenterBuildRecord = serde_json::from_slice(&bundle::read_bounded(&record_path, 64 * 1024)?)?;
    record.validate(repo, &presenter, &manifest)?;

    let stage = tempfile::Builder::new().prefix(".droidloom-derive-")
        .tempdir_in(destination.parent().unwrap())?;
    copy_base(&base, stage.path(), &manifest)?;
    copy(&presenter, &stage.path().join(bundle::PRESENTER))?;

    let mut next = manifest.clone();
    next.build_id = bundle::new_build_id()?;
    next.kind = BundleKind::PresenterDerivative;
    next.base_build_id = Some(manifest.build_id.clone());
    next.source_identity = format!("base:{} presenter:{}", manifest.build_id, record.source_commit);
    let entry = next.files.iter_mut().find(|a| a.path == bundle::PRESENTER)
        .ok_or("base is missing its presenter file")?;
    let staged = stage.path().join(bundle::PRESENTER);
    let metadata = fs::symlink_metadata(&staged)?;
    entry.size = metadata.len();
    entry.mode = metadata.mode() & 0o777;
    entry.sha256 = record.presenter_sha256.clone();
    let component = next.components.iter_mut().find(|c| c.name == "presenter")
        .ok_or("base is missing its presenter component")?;
    component.source_commit = record.source_commit.clone();
    component.cargo_lock_sha256 = Some(record.cargo_lock_sha256.clone());
    component.source_lock_sha256 = Some(record.source_lock_sha256.clone());
    component.target = record.target.clone();
    component.toolchain = record.toolchain.clone();
    component.build_profile = record.build_profile.clone();

    bundle::write_manifest(stage.path(), &next)?;
    bundle::verify(stage.path())?;
    // Source movement after record validation must not be silently attributed.
    if bundle::clean_source_commit(repo)? != record.source_commit {
        return fail("source changed while creating derivative");
    }
    sync_tree(stage.path())?;
    publish_directory(stage.path(), &destination)?;
    Ok(next.build_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        output(Command::new("git").current_dir(repo)
            .args(["-c", "user.name=Bundle Tests", "-c", "user.email=bundle-tests@example.invalid"])
            .args(args)).unwrap()
    }

    fn repository(root: &Path) -> PathBuf {
        let repo = root.join("source");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        write(&repo.join("Cargo.lock"), b"locked dependencies").unwrap();
        write(&repo.join("android/manifest/m2-sparse-source-lock.json"), b"locked android").unwrap();
        write(&repo.join("graphics/droidloom-wayland/src/main.rs"), b"baseline").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "baseline"]);
        repo
    }

    fn setup(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, Manifest) {
        let repo = repository(root);
        let base = root.join("base");
        bundle::artifact_tests::fixture(&base, 7, 7);
        let provenance = BuildProvenance::capture(&repo, "x86_64").unwrap();
        bundle::seal(&base, &provenance).unwrap();
        let manifest = bundle::verify(&base).unwrap();
        write(&repo.join("graphics/droidloom-wayland/src/main.rs"), b"new presenter").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "presenter change"]);
        let presenter = root.join("presenter");
        let mut bytes = fs::read(base.join(bundle::PRESENTER)).unwrap();
        bytes.extend_from_slice(b"new release");
        write(&presenter, bytes).unwrap();
        mode(&presenter, 0o755).unwrap();
        let record = root.join("build.json");
        PresenterBuildRecord::write_after_build(&repo, &BuildProvenance::capture(&repo, "x86_64").unwrap(), &presenter, &record,
            vec!["build".into(), "--locked".into(), "--release".into()]).unwrap();
        (repo, base, presenter, record, manifest)
    }

    #[test]
    fn derivative_keeps_base_bytes_and_component_sources() {
        let d = tempfile::tempdir().unwrap();
        let (repo, base, presenter, record, original) = setup(d.path());
        let destination = d.path().join("derivative");
        run(&repo, &base, &presenter, &record, &destination).unwrap();
        let derived = bundle::verify(&destination).unwrap();
        assert_eq!(derived.kind, BundleKind::PresenterDerivative);
        assert_eq!(derived.base_build_id.as_deref(), Some(original.build_id.as_str()));
        for artifact in &original.files {
            if artifact.path != bundle::PRESENTER {
                assert_eq!(derived.files.iter().find(|a| a.path == artifact.path), Some(artifact));
            }
        }
        assert_eq!(derived.components.iter().find(|c| c.name == "android"), original.components.iter().find(|c| c.name == "android"));
        assert_ne!(derived.components.iter().find(|c| c.name == "presenter"), original.components.iter().find(|c| c.name == "presenter"));
        assert_eq!(bundle::verify(&base).unwrap(), original);
        assert!(run(&repo, &base, &presenter, &record, &destination).is_err());
    }

    #[test]
    fn only_presenter_source_changes_are_allowed() {
        assert!(allowed_source_path("graphics/droidloom-wayland/src/chrome.rs"));
        assert!(allowed_source_path("docs/integration.md"));
        for path in ["Cargo.lock", "graphics/droidloom-wayland/Cargo.toml", "graphics/droidloom-wayland/build.rs", ".cargo/config.toml", "runtime/droidloom-supervisor/src/lib.rs", "android/device/product.mk", "graphics/droidloom-denial-protocol/src/lib.rs", "graphics/droidloom-wayland/src/../Cargo.toml"] {
            assert!(!allowed_source_path(path), "{path}");
        }
        let d = tempfile::tempdir().unwrap();
        let (repo, base, presenter, record, _) = setup(d.path());
        write(&repo.join("runtime/service.rs"), b"ABI change").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "other component"]);
        PresenterBuildRecord::write_after_build(&repo, &BuildProvenance::capture(&repo, "x86_64").unwrap(), &presenter, &record,
            vec!["--locked".into(), "--release".into()]).unwrap();
        assert!(run(&repo, &base, &presenter, &record, &d.path().join("out")).is_err());
    }

    #[test]
    fn rejects_dirty_source_wrong_record_and_wrong_architecture() {
        let d = tempfile::tempdir().unwrap();
        let (repo, base, presenter, record, _) = setup(d.path());
        write(&repo.join("untracked"), b"dirty").unwrap();
        assert!(run(&repo, &base, &presenter, &record, &d.path().join("out")).is_err());
        fs::remove_file(repo.join("untracked")).unwrap();
        let mut data: PresenterBuildRecord = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        data.presenter_sha256 = "0".repeat(64);
        durable_write(&record, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(run(&repo, &base, &presenter, &record, &d.path().join("out")).is_err());
        let mut bytes = fs::read(&presenter).unwrap();
        bytes[18] = 183;
        write(&presenter, bytes).unwrap();
        data.presenter_sha256 = hash(&presenter).unwrap();
        durable_write(&record, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(run(&repo, &base, &presenter, &record, &d.path().join("out")).is_err());
    }

    #[test]
    fn copy_does_not_bless_a_mutated_base() {
        let d = tempfile::tempdir().unwrap();
        let (_, base, _, _, manifest) = setup(d.path());
        let relative = "usr/lib/droidloom/runtime/ime/home-setup";
        write(&base.join(relative), b"changed base").unwrap();
        let stage = d.path().join("stage");
        fs::create_dir(&stage).unwrap();
        copy_base(&base, &stage, &manifest).unwrap();
        bundle::write_manifest(&stage, &manifest).unwrap();
        assert!(bundle::verify(&stage).is_err());
    }
}
