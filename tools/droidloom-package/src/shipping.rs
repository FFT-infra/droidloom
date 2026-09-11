//! Small release interface shared by local runs and GitHub Actions.
use super::{Result, Version, repository, run};
use clap::Subcommand;
use std::{fs, path::Path, process::Command};

const REPOSITORY: &str = "denialwm/droidloom";
const RELEASE: &str = "packages";

#[derive(Subcommand)]
pub enum Action {
    /// Write the Pages database and installer from the current package pair.
    Prepare,
    /// Upload immutable package archives before deploying the Pages database.
    Publish,
}

fn packages(repo: &Path, version: &Version) -> Result<Vec<std::path::PathBuf>> {
    ["runtime", "image"]
        .iter()
        .map(|component| {
            let path = repo
                .join("dist/arch")
                .join(version.directory())
                .join(format!(
                    "droidloom-{component}-{}-x86_64.pkg.tar.zst",
                    version.directory()
                ));
            if !path.is_file() {
                return Err(format!("missing {}", path.display()).into());
            }
            if fs::metadata(&path)?.len() >= 2 * 1024 * 1024 * 1024 {
                return Err("GitHub Release assets must be smaller than 2 GiB".into());
            }
            Ok(path)
        })
        .collect()
}

fn gh() -> Command {
    let mut command = Command::new("gh");
    command.env("GH_REPO", REPOSITORY);
    command
}

pub fn execute(action: Action) -> Result<()> {
    let repo = repository(None)?;
    let version = Version::read(&repo)?;
    let packages = packages(&repo, &version)?;
    let site = repo.join("dist/pages");
    match action {
        Action::Prepare => {
            if site.exists() {
                fs::remove_dir_all(&site)?;
            }
            let database = site.join("x86_64");
            fs::create_dir_all(&database)?;
            run(Command::new("repo-add")
                .arg(database.join("droidloom.db.tar.gz"))
                .args(&packages))?;
            // Pages artifacts cannot contain symlinks. Publish the actual bytes.
            for name in ["droidloom.db", "droidloom.files"] {
                fs::remove_file(database.join(name))?;
                fs::copy(database.join(format!("{name}.tar.gz")), database.join(name))?;
            }
            fs::copy(repo.join("install.sh"), site.join("install.sh"))?;
            fs::write(site.join(".nojekyll"), "")?;
            fs::write(
                site.join("index.html"),
                format!(
                    "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Droidloom packages</title>\n<h1>Droidloom {}</h1><p>Arch Linux / Omarchy, x86_64.</p>\n<p>Download and review <a href=\"install.sh\">install.sh</a>, then run <code>sh install.sh</code> to install or update.</p>\n<p>Packages are unsigned and delivered over HTTPS. The installer configures this policy only for the Droidloom repository.</p>\n<p><a href=\"https://github.com/denialwm/droidloom/blob/main/docs/INSTALL.md\">Installation guide</a> · <a href=\"https://github.com/denialwm/droidloom/releases/tag/packages\">Packages and source</a></p></html>\n",
                    version.directory()
                ),
            )?;
        }
        Action::Publish => {
            let metadata = gh()
                .args(["api", "repos/denialwm/droidloom/releases/tags/packages"])
                .output()?;
            let release: serde_json::Value = if metadata.status.success() {
                serde_json::from_slice(&metadata.stdout)?
            } else {
                // A failed create remains an error (including authentication failures).
                run(gh().args(["release", "create", RELEASE, "--title", "Droidloom pacman packages", "--notes", "Pacman package archives and matching source snapshots. Install using https://denialwm.github.io/droidloom/install.sh. Older archives are retained for clients with cached databases."]))?;
                serde_json::json!({"assets": []})
            };
            let source = repo
                .join("dist")
                .join(format!("droidloom-source-{}.tar.gz", version.directory()));
            run(Command::new("git")
                .current_dir(&repo)
                .args(["archive", "--format=tar.gz", "--prefix=droidloom/"])
                .arg("-o")
                .arg(&source)
                .arg("HEAD"))?;
            // Preflight every asset before any upload. Same-version rebuilds may
            // resume only when their bytes match; never replace published files.
            let mut pending = Vec::new();
            for path in packages.iter().chain(std::iter::once(&source)) {
                let name = path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .ok_or("non-UTF8 filename")?;
                if release["assets"]
                    .as_array()
                    .is_some_and(|assets| assets.iter().any(|a| a["name"].as_str() == Some(name)))
                {
                    let temporary = tempfile::tempdir()?;
                    run(gh()
                        .args(["release", "download", RELEASE, "--pattern", name, "--dir"])
                        .arg(temporary.path()))?;
                    let status = Command::new("cmp")
                        .arg("--silent")
                        .arg(path)
                        .arg(temporary.path().join(name))
                        .status()?;
                    if !status.success() {
                        return Err(format!("{name} is already published with different bytes; increase packaging/arch/version.json release").into());
                    }
                } else {
                    pending.push(path);
                }
            }
            for path in pending {
                run(gh().args(["release", "upload", RELEASE]).arg(path))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_requires_exact_pair_and_excludes_addons() {
        let temp = tempfile::tempdir().unwrap();
        let version = Version {
            version: "1.2.3".into(),
            release: 4,
            architecture: "x86_64".into(),
        };
        let output = temp.path().join("dist/arch/1.2.3-4");
        fs::create_dir_all(&output).unwrap();
        fs::write(
            output.join("droidloom-runtime-1.2.3-4-x86_64.pkg.tar.zst"),
            "runtime",
        )
        .unwrap();
        assert!(packages(temp.path(), &version).is_err());
        fs::write(
            output.join("droidloom-image-1.2.3-4-x86_64.pkg.tar.zst"),
            "image",
        )
        .unwrap();
        fs::write(output.join("droidloom-gapps-1-x86_64.pkg.tar.zst"), "addon").unwrap();
        assert_eq!(packages(temp.path(), &version).unwrap().len(), 2);
    }
}
