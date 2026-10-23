//! Precompiled system-server code for Droidloom's services.jar.
//!
//! The base image precompiles every system-server jar. Each odex records the
//! class-loader context it was compiled in, including the checksum of every
//! jar loaded before it. Droidloom replaces services.jar, so ART rejects
//! services.odex and the odex of every jar after it, and system_server runs
//! interpreted. This compiles those jars again the way the base build did,
//! against the base image's own boot image and jars, and stages a complete
//! replacement of `/system/framework/oat/<isa>`.
use crate::util::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const SERVICES: &str = "/system/framework/services.jar";
// The header layout read below is that of this version (art/runtime/oat/oat.h).
const OAT_VERSION: &[u8; 4] = b"275\0";

/// What an oat file says about how it was compiled.
#[derive(Debug)]
struct Oat {
    /// Location of the first dex file it compiles.
    location: String,
    keys: BTreeMap<String, String>,
}

impl Oat {
    fn key(&self, name: &str) -> Result<&str> {
        self.keys
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("oat file of {} has no {name}", self.location).into())
    }

    /// The stored class-loader context without dex checksums.
    fn context(&self) -> Result<String> {
        let mut context = String::new();
        let mut checksum = false;
        for c in self.key("classpath")?.chars() {
            match c {
                '*' => checksum = true,
                ':' | ']' | '[' | ';' | '{' | '}' => {
                    checksum = false;
                    context.push(c);
                }
                _ if !checksum => context.push(c),
                _ => {}
            }
        }
        Ok(context)
    }
}

fn read_oat(path: &Path) -> Result<Oat> {
    let bytes = fs::read(path)?;
    let at = |offset: usize| -> Option<usize> {
        let field = bytes.get(offset..offset + 4)?;
        Some(u32::from_le_bytes(field.try_into().ok()?) as usize)
    };
    // The oat header records its own offset in the ELF file.
    let header = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, magic)| *magic == b"oat\n")
        .map(|(offset, _)| offset)
        .find(|&offset| at(offset + 32) == Some(offset))
        .ok_or_else(|| format!("{} has no oat header", path.display()))?;
    if bytes.get(header + 4..header + 8) != Some(OAT_VERSION) {
        return fail(format!("{} is not an oat version 275 file", path.display()));
    }
    let malformed = || format!("{} has a malformed oat header", path.display());
    let store_size = at(header + 68).ok_or_else(malformed)?;
    let store = bytes
        .get(header + 72..header + 72 + store_size)
        .ok_or_else(malformed)?;
    let mut fields = store.split(|&b| b == 0).map(String::from_utf8_lossy);
    let mut keys = BTreeMap::new();
    while let (Some(key), Some(value)) = (fields.next(), fields.next()) {
        keys.insert(key.into_owned(), value.into_owned());
    }
    let dex_files = header + at(header + 24).ok_or_else(malformed)?;
    let length = at(dex_files).ok_or_else(malformed)?;
    let location = bytes
        .get(dex_files + 4..dex_files + 4 + length)
        .ok_or_else(malformed)?;
    Ok(Oat {
        location: String::from_utf8(location.to_vec())?,
        keys,
    })
}

/// Files of the base partitions, extracted on demand.
struct Base<'a> {
    images: &'a Path,
    services: &'a Path,
    root: PathBuf,
}

impl Base<'_> {
    /// Extract `path`, an Android path to a file or directory, unless it is
    /// already extracted. Returns false when no base partition has it.
    fn extract(&self, path: &str) -> Result<bool> {
        let (image, inner) = if let Some(rest) = path.strip_prefix("/system_ext/") {
            ("system_ext.img", format!("/{rest}"))
        } else if let Some(rest) = path.strip_prefix("/product/") {
            ("product.img", format!("/{rest}"))
        } else {
            ("system.img", path.to_owned())
        };
        let destination = self.root.join(path.trim_start_matches('/'));
        if destination.exists() {
            return Ok(true);
        }
        fs::create_dir_all(destination.parent().ok_or("path has no parent")?)?;
        let status = Command::new("fsck.erofs")
            .arg(format!("--extract={}", destination.display()))
            .arg(format!("--path={inner}"))
            .arg(self.images.join(image))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }

    /// The host file of an Android jar location.
    fn jar(&self, location: &str) -> Result<PathBuf> {
        if location == SERVICES {
            return Ok(self.services.to_owned());
        }
        let Some(apex) = location.strip_prefix("/apex/") else {
            if !self.extract(location)? {
                return fail(format!("the base image has no {location}"));
            }
            return Ok(self.root.join(location.trim_start_matches('/')));
        };
        let (name, inner) = apex.split_once('/').ok_or("malformed APEX jar location")?;
        let payload = self.root.join("apex").join(name);
        if !payload.exists() {
            self.extract_apex(name, &payload)?;
        }
        Ok(payload.join(inner))
    }

    fn extract_apex(&self, name: &str, payload: &Path) -> Result<()> {
        let candidates = ["/system/apex", "/system_ext/apex", "/product/apex"]
            .into_iter()
            .flat_map(|dir| [format!("{dir}/{name}.apex"), format!("{dir}/{name}.capex")]);
        let mut found = None;
        for candidate in candidates {
            if self.extract(&candidate)? {
                found = Some(self.root.join(candidate.trim_start_matches('/')));
                break;
            }
        }
        let mut archive = found.ok_or_else(|| format!("the base image has no {name} APEX"))?;
        if archive.extension().is_some_and(|e| e == "capex") {
            let original = archive.with_extension("apex");
            unzip(&archive, "original_apex", &original)?;
            archive = original;
        }
        let image = self.root.join("apex").join(format!("{name}.img"));
        fs::create_dir_all(payload)?;
        unzip(&archive, "apex_payload.img", &image)?;
        let header = fs::read(&image)?.get(1024..1082).map(<[u8]>::to_vec);
        match header {
            Some(h) if h[56..58] == [0x53, 0xef] => {
                run(Command::new("debugfs")
                    .arg("-R")
                    .arg(format!("rdump /javalib {}", payload.display()))
                    .arg(&image))?;
            }
            Some(h) if h[..4] == [0xe2, 0xe1, 0xf5, 0xe0] => {
                run(Command::new("fsck.erofs")
                    .arg(format!("--extract={}", payload.display()))
                    .arg(&image))?;
            }
            _ => return fail(format!("{name} has an unknown payload filesystem")),
        }
        fs::remove_file(image)?;
        Ok(())
    }
}

fn unzip(archive: &Path, member: &str, destination: &Path) -> Result<()> {
    let file = fs::File::create(destination)?;
    run(Command::new("unzip")
        .arg("-p")
        .arg(archive)
        .arg(member)
        .stdout(file))
}

/// Recompile, into `destination/<isa>`, every base system-server odex that
/// depends on services.jar, and copy every other base oat file unchanged.
/// `tools` holds the host `dex2oat64` built from the base image's source.
pub fn stage(
    images: &Path,
    services: &Path,
    tools: &Path,
    scratch: &Path,
    destination: &Path,
) -> Result<()> {
    let base = Base {
        images,
        services,
        root: scratch.to_owned(),
    };
    if !base.extract("/system/framework")? {
        return fail("the base image has no /system/framework");
    }
    let framework = scratch.join("system/framework");
    let empty = scratch.join("empty");
    fs::create_dir_all(&empty)?;
    for isa in fs::read_dir(framework.join("oat"))? {
        let isa = isa?;
        let name = isa.file_name().into_string().map_err(|_| "bad ISA name")?;
        let output = destination.join(&name);
        fs::create_dir_all(&output)?;
        let mut compile = Vec::new();
        for entry in fs::read_dir(isa.path())? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "odex") {
                let oat = read_oat(&path)?;
                if oat.location == SERVICES
                    || oat.key("classpath")?.contains(&format!("{SERVICES}*"))
                {
                    compile.push((path, oat));
                }
            }
        }
        let replaced = |file: &str| {
            compile.iter().any(|(odex, _)| {
                let stem = odex.file_stem().unwrap().to_string_lossy();
                file.strip_prefix(stem.as_ref())
                    .is_some_and(|rest| rest.starts_with('.'))
            })
        };
        for entry in fs::read_dir(isa.path())? {
            let entry = entry?;
            let file = entry.file_name().to_string_lossy().into_owned();
            if !replaced(&file) {
                copy(&entry.path(), &output.join(&file))?;
            }
        }
        for (odex, oat) in &compile {
            let target = output.join(odex.file_name().unwrap());
            dex2oat(&base, tools, &empty, &framework, &name, odex, oat, &target)?;
            let compiled = read_oat(&target)?;
            for key in [
                "bootclasspath-checksums",
                "apex-versions",
                "compiler-filter",
            ] {
                if compiled.key(key)? != oat.key(key)? {
                    return fail(format!(
                        "{} compiled with {key} {}, the base has {}",
                        oat.location,
                        compiled.key(key)?,
                        oat.key(key)?
                    ));
                }
            }
            if compiled.context()? != oat.context()? {
                return fail(format!("{} compiled in another context", oat.location));
            }
        }
        println!(
            "Compiled {} system-server jars for {name} against the base boot image",
            compile.len()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dex2oat(
    base: &Base<'_>,
    tools: &Path,
    empty: &Path,
    framework: &Path,
    isa: &str,
    odex: &Path,
    oat: &Oat,
    target: &Path,
) -> Result<()> {
    let boot = oat.key("bootclasspath")?.split(':').collect::<Vec<_>>();
    let host_boot = boot
        .iter()
        .map(|jar| Ok(base.jar(jar)?.display().to_string()))
        .collect::<Result<Vec<_>>>()?;
    // A boot image in parts names each later part after its first jar.
    let mut images = Vec::new();
    let mut first = 0;
    for part in oat.key("bootclasspath-checksums")?.split(':') {
        let count = part
            .strip_prefix("i;")
            .and_then(|rest| rest.split('/').next())
            .and_then(|count| count.parse::<usize>().ok())
            .ok_or("unsupported boot class path checksums")?;
        let stem = match first {
            0 => "boot".to_owned(),
            _ => {
                let jar = Path::new(boot.get(first).ok_or("boot image exceeds class path")?);
                format!("boot-{}", jar.file_stem().unwrap().to_string_lossy())
            }
        };
        images.push(framework.join(format!("{stem}.art")).display().to_string());
        first += count;
    }
    let context = oat.context()?;
    let mut host_context = String::new();
    let mut token = String::new();
    for c in context.chars().chain(std::iter::once(']')) {
        if matches!(c, ':' | ']' | '[' | ';') {
            if !token.is_empty() && token != "PCL" {
                host_context.push_str(&base.jar(&token)?.display().to_string());
            } else {
                host_context.push_str(&token);
            }
            token.clear();
            host_context.push(c);
        } else {
            token.push(c);
        }
    }
    host_context.pop();
    let jar = base.jar(&oat.location)?;
    let mut command = Command::new(tools.join("dex2oat64"));
    command
        .arg("--avoid-storing-invocation")
        .args(["--runtime-arg", "-Xms64m", "--runtime-arg", "-Xmx512m"])
        .arg("--runtime-arg")
        .arg(format!("-Xbootclasspath:{}", host_boot.join(":")))
        .arg("--runtime-arg")
        .arg(format!("-Xbootclasspath-locations:{}", boot.join(":")))
        .arg(format!("--class-loader-context={host_context}"))
        .arg(format!("--stored-class-loader-context={context}"))
        .arg(format!("--boot-image={}", images.join(":")))
        .arg(format!("--dex-file={}", jar.display()))
        .arg(format!("--dex-location={}", oat.location))
        .arg(format!("--oat-file={}", target.display()))
        .arg(format!("--android-root={}", empty.display()))
        .arg(format!("--instruction-set={isa}"))
        .args([
            "--instruction-set-variant=generic",
            "--instruction-set-features=default",
            "--generate-mini-debug-info",
            "--generate-build-id",
            "--abort-on-hard-verifier-error",
            "--force-determinism",
            "--no-inline-from=core-oj.jar",
            "--copy-dex-files=false",
            "--compilation-reason=prebuilt",
        ])
        .arg(format!("--compiler-filter={}", oat.key("compiler-filter")?));
    if oat.key("concurrent-copying")? == "false" {
        command.args(["--runtime-arg", "-Xgc:CMC"]);
    }
    if let Some(sdk) = oat.keys.get("assume-value-sdk-int") {
        command.arg(format!(
            "--assume-value=Landroid/os/Build$VERSION;->SDK_INT:{sdk}"
        ));
    }
    if oat.keys.get("enable-profile-code").map(String::as_str) == Some("true") {
        command.arg("--allow-profile-code");
    }
    let profile = PathBuf::from(format!("{}.prof", jar.display()));
    if profile.is_file() {
        command.arg(format!("--profile-file={}", profile.display()));
    } else if oat.key("compiler-filter")? == "speed-profile" {
        return fail(format!("{} has no profile", oat.location));
    }
    if odex.with_extension("art").is_file() {
        command
            .arg(format!(
                "--app-image-file={}",
                target.with_extension("art").display()
            ))
            .args(["--image-format=lz4", "--resolve-startup-const-strings=true"]);
    }
    run_build(&mut command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oat_file(location: &str, keys: &[(&str, &str)]) -> Vec<u8> {
        let header = 0x40;
        let mut store = Vec::new();
        for (key, value) in keys {
            store.extend_from_slice(key.as_bytes());
            store.push(0);
            store.extend_from_slice(value.as_bytes());
            store.push(0);
        }
        let mut bytes = vec![0; header];
        // An earlier "oat\n" that is not the header must be skipped.
        bytes[8..12].copy_from_slice(b"oat\n");
        let mut fields = [0u32; 17];
        fields[6] = (72 + store.len()) as u32;
        fields[8] = header as u32;
        bytes.extend_from_slice(b"oat\n");
        bytes.extend_from_slice(OAT_VERSION);
        for field in &fields[2..] {
            bytes.extend_from_slice(&field.to_le_bytes());
        }
        bytes.extend_from_slice(&(store.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&store);
        bytes.extend_from_slice(&(location.len() as u32).to_le_bytes());
        bytes.extend_from_slice(location.as_bytes());
        bytes
    }

    #[test]
    fn reads_the_location_and_keys_of_an_oat_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("services.odex");
        let context = "PCL[];PCL[/system/framework/a.jar*1154147141:/apex/b/javalib/c.jar*42]";
        fs::write(
            &path,
            oat_file(
                SERVICES,
                &[("classpath", context), ("compiler-filter", "speed-profile")],
            ),
        )
        .unwrap();
        let oat = read_oat(&path).unwrap();
        assert_eq!(oat.location, SERVICES);
        assert_eq!(oat.key("compiler-filter").unwrap(), "speed-profile");
        assert_eq!(
            oat.context().unwrap(),
            "PCL[];PCL[/system/framework/a.jar:/apex/b/javalib/c.jar]"
        );
    }

    #[test]
    fn rejects_another_oat_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.odex");
        let mut bytes = oat_file(SERVICES, &[]);
        bytes[0x44..0x48].copy_from_slice(b"259\0");
        fs::write(&path, bytes).unwrap();
        assert!(read_oat(&path).is_err());
    }
}
