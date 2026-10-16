//! Mount package-owned APEX compatibility inputs before Android derives its
//! boot class path.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

const DERIVE_CLASSPATH: &str = "/apex/com.android.sdkext/bin/derive_classpath";
const TOYBOX: &str = "/system/bin/toybox";

fn main() -> ExitCode {
    match arguments().and_then(|config| run(&config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("droidloom-classpath-wrapper: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Config {
    projection: PathBuf,
    module: String,
    file_overrides: Vec<FileOverride>,
}

#[derive(Debug, Eq, PartialEq)]
struct FileOverride {
    source: PathBuf,
    target: PathBuf,
}

fn arguments() -> Result<Config, String> {
    parse_arguments(env::args().skip(1))
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let mut arguments = arguments.into_iter();
    let projection = arguments.next().ok_or_else(usage)?;
    let module = arguments.next().ok_or_else(usage)?;
    let projection = PathBuf::from(projection);
    if !is_normal_absolute(&projection) || !projection.starts_with("/droidloom") {
        return Err("projection must be a normalized absolute path below /droidloom".into());
    }
    if module.is_empty()
        || !module
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("module must contain only ASCII letters, digits, '.', '_' or '-'".into());
    }

    let mut file_overrides = Vec::new();
    while let Some(flag) = arguments.next() {
        if flag != "--file-override" {
            return Err(usage());
        }
        let source = PathBuf::from(arguments.next().ok_or_else(usage)?);
        let target = PathBuf::from(arguments.next().ok_or_else(usage)?);
        if !is_below(&source, Path::new("/droidloom")) {
            return Err(
                "file override source must be a normalized absolute path below /droidloom".into(),
            );
        }
        if !is_below(&target, Path::new("/apex")) {
            return Err(
                "file override target must be a normalized absolute path below /apex".into(),
            );
        }
        file_overrides.push(FileOverride { source, target });
    }

    Ok(Config {
        projection,
        module,
        file_overrides,
    })
}

fn usage() -> String {
    "usage: droidloom-classpath-wrapper PROJECTION APEX_MODULE [--file-override SOURCE TARGET]..."
        .into()
}

fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn is_below(path: &Path, root: &Path) -> bool {
    is_normal_absolute(path) && path != root && path.starts_with(root)
}

fn run(config: &Config) -> Result<(), String> {
    let source = fs::canonicalize(&config.projection)
        .map_err(|error| format!("resolve {}: {error}", config.projection.display()))?;
    if source != config.projection {
        return Err(format!(
            "projection {} resolves outside its declared path",
            config.projection.display()
        ));
    }
    if !fs::metadata(&source)
        .map_err(|error| format!("stat {}: {error}", source.display()))?
        .is_dir()
    {
        return Err(format!("{} is not a directory", source.display()));
    }

    // Resolve and validate every file before performing the first mount. A
    // malformed override must not leave a partially constructed class path.
    let file_overrides = config
        .file_overrides
        .iter()
        .map(resolve_file_override)
        .collect::<Result<Vec<_>, _>>()?;

    let target = Path::new("/apex").join(&config.module);
    match fs::create_dir(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::metadata(&target)
                .map_err(|source| format!("stat {}: {source}", target.display()))?
                .is_dir()
            {
                return Err(format!("{} is not a directory", target.display()));
            }
        }
        Err(error) => return Err(format!("create {}: {error}", target.display())),
    }

    // APEX metadata names the versioned mount as well as the public alias.
    // Leaving that mount intact advertises HALs whose binaries this projection
    // hides (notably AVF remote provisioning), causing endless lazy starts.
    let mut targets = versioned_targets(Path::new("/apex"), &config.module)?;
    targets.push(target);
    for target in targets {
        checked(
            Command::new(TOYBOX)
                .args(["mount", "--bind"])
                .arg(&source)
                .arg(&target)
                .status(),
            "bind classpath projection",
        )?;
    }

    for file_override in file_overrides {
        checked(
            Command::new(TOYBOX)
                .args(["mount", "--bind"])
                .arg(&file_override.source)
                .arg(&file_override.target)
                .status(),
            "bind APEX file override",
        )?;
    }
    // libvintf may have cached the original APEX manifest before this oneshot.
    // Its cache key is the APEX info-list mtime. The projection changes what
    // that list resolves to, so refresh the key without changing its contents.
    let apex_info = Path::new("/apex/apex-info-list.xml");
    if apex_info.exists() {
        checked(
            Command::new(TOYBOX).arg("touch").arg(apex_info).status(),
            "invalidate projected APEX manifest cache",
        )?;
    }

    // The supervisor has already made the source projection read-only,
    // nosuid, and nodev. A bind of that mount preserves the restrictions.
    // Remounting through Android toybox would resolve the backing host block
    // device and fail with EBUSY even though the target is already read-only.
    checked(
        Command::new(DERIVE_CLASSPATH).status(),
        "derive Android class paths",
    )
}

// Match exact module version aliases only, never another APEX or a symlink.
fn versioned_targets(root: &Path, module: &str) -> Result<Vec<PathBuf>, String> {
    let prefix = format!("{module}@");
    let mut targets = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| format!("read APEX mounts: {error}"))? {
        let entry = entry.map_err(|error| format!("read APEX entry: {error}"))?;
        let name = entry.file_name();
        let Some(version) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if !entry
            .file_type()
            .map_err(|error| format!("stat APEX entry: {error}"))?
            .is_dir()
        {
            return Err(format!(
                "versioned APEX {} is not a directory",
                entry.path().display()
            ));
        }
        targets.push(entry.path());
    }
    targets.sort();
    Ok(targets)
}

#[derive(Debug)]
struct ResolvedFileOverride {
    source: PathBuf,
    target: PathBuf,
}

fn resolve_file_override(file_override: &FileOverride) -> Result<ResolvedFileOverride, String> {
    let source = fs::canonicalize(&file_override.source)
        .map_err(|error| format!("resolve {}: {error}", file_override.source.display()))?;
    if source != file_override.source {
        return Err(format!(
            "file override source {} resolves outside its declared path",
            file_override.source.display()
        ));
    }
    if !fs::metadata(&source)
        .map_err(|error| format!("stat {}: {error}", source.display()))?
        .is_file()
    {
        return Err(format!("{} is not a regular file", source.display()));
    }

    let resolved_target = fs::canonicalize(&file_override.target)
        .map_err(|error| format!("resolve {}: {error}", file_override.target.display()))?;
    if !is_below(&resolved_target, Path::new("/apex")) {
        return Err(format!(
            "file override target {} resolves outside /apex",
            file_override.target.display()
        ));
    }
    if !fs::metadata(&resolved_target)
        .map_err(|error| format!("stat {}: {error}", resolved_target.display()))?
        .is_file()
    {
        return Err(format!(
            "{} is not a regular file",
            file_override.target.display()
        ));
    }

    Ok(ResolvedFileOverride {
        source,
        target: file_override.target.clone(),
    })
}

fn checked(result: std::io::Result<ExitStatus>, operation: &str) -> Result<(), String> {
    let status = result.map_err(|error| format!("{operation}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{operation} failed ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_covers_only_exact_versioned_module_directories() {
        let root =
            std::env::temp_dir().join(format!("droidloom-apex-alias-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        for name in [
            "com.android.virt@37",
            "com.android.virt@38",
            "com.android.virt",
            "com.android.virt.other@37",
            "com.android.virt@",
            "com.android.virt@bad",
        ] {
            fs::create_dir(root.join(name)).unwrap();
        }
        assert_eq!(
            versioned_targets(&root, "com.android.virt").unwrap(),
            vec![
                root.join("com.android.virt@37"),
                root.join("com.android.virt@38")
            ]
        );
        std::os::unix::fs::symlink(
            root.join("com.android.virt@37"),
            root.join("com.android.virt@39"),
        )
        .unwrap();
        assert!(versioned_targets(&root, "com.android.virt").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_one_bounded_projection() {
        assert_eq!(
            parse_arguments([
                "/droidloom/classpath/apex/com.android.virt".into(),
                "com.android.virt".into(),
            ])
            .unwrap(),
            Config {
                projection: "/droidloom/classpath/apex/com.android.virt".into(),
                module: "com.android.virt".into(),
                file_overrides: Vec::new(),
            }
        );
    }

    #[test]
    fn accepts_bounded_apex_file_overrides() {
        assert_eq!(
            parse_arguments([
                "/droidloom/classpath/apex/com.android.virt".into(),
                "com.android.virt".into(),
                "--file-override".into(),
                "/droidloom/classpath/lib64/libservice-connectivity.so".into(),
                "/apex/com.android.tethering/lib64/libservice-connectivity.so".into(),
            ])
            .unwrap()
            .file_overrides,
            vec![FileOverride {
                source: "/droidloom/classpath/lib64/libservice-connectivity.so".into(),
                target: "/apex/com.android.tethering/lib64/libservice-connectivity.so".into(),
            }]
        );
    }

    #[test]
    fn rejects_escape_and_extra_arguments() {
        assert!(parse_arguments(["/vendor/escape".into(), "com.android.virt".into()]).is_err());
        assert!(parse_arguments(["/droidloom/classpath".into(), "../escape".into(),]).is_err());
        assert!(
            parse_arguments([
                "/droidloom/classpath".into(),
                "com.android.virt".into(),
                "extra".into(),
            ])
            .is_err()
        );
        assert!(
            parse_arguments([
                "/droidloom/classpath".into(),
                "com.android.virt".into(),
                "--file-override".into(),
                "/system/lib64/escape.so".into(),
                "/apex/com.android.tethering/lib64/escape.so".into(),
            ])
            .is_err()
        );
        assert!(
            parse_arguments([
                "/droidloom/classpath".into(),
                "com.android.virt".into(),
                "--file-override".into(),
                "/droidloom/classpath/escape.so".into(),
                "/system/lib64/escape.so".into(),
            ])
            .is_err()
        );
    }
}
