use crate::{archive, util::*, xml};
use droidloom_contracts::{
    Architecture,
    gapps::{Apk, CORE_APPS, FileDigest, SDK, SYNC_APPS},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Imported {
    pub architecture: Architecture,
    pub version: String,
    pub archive: FileDigest,
    pub apks: Vec<Apk>,
    pub files: BTreeMap<String, FileDigest>,
    pub tree: PathBuf,
    pub license: Vec<u8>,
    pub excluded: Vec<String>,
}

pub fn prepare(
    archive_path: &Path,
    expected: &str,
    work: &Path,
    aapt2: &Path,
    apksigner: &Path,
    sync: bool,
) -> Result<Imported> {
    if fs::metadata(archive_path)?.len() > archive::MAX_ARCHIVE {
        return Err("LiteGapps archive exceeds 512 MiB".into());
    }
    let digest = FileDigest::read(archive_path)?;
    if !droidloom_contracts::gapps::is_sha256(expected)
        || digest.sha256 != expected
        || digest.size > archive::MAX_ARCHIVE
    {
        return Err("LiteGapps archive checksum or size does not match the supplied input".into());
    }
    // Work exclusively on a private copy, verified after copying as well.
    let archive = work.join("source.zip");
    fs::copy(archive_path, &archive)?;
    if FileDigest::read(&archive)? != digest {
        return Err("archive changed during import".into());
    }
    let outer = archive::list(&archive)?;
    for file in ["module.prop", "LICENSE", "files/files.tar.xz"] {
        if outer.get(file) != Some(&false) {
            return Err(format!("missing regular archive input: {file}").into());
        }
    }
    let props = archive::properties(&String::from_utf8(archive::member(
        &archive,
        "module.prop",
        16_384,
    )?)?)?;
    if props.get("id").map(String::as_str) != Some("litegapps")
        || props.get("litegapps_type").map(String::as_str) != Some("litegapps_regular")
        || props.get("litegapps_variant").map(String::as_str) != Some("lite")
    {
        return Err("only the LiteGapps regular lite variant is supported".into());
    }
    let version = props
        .get("version")
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .ok_or("missing LiteGapps version")?
        .clone();
    let compressed = work.join("payload.tar.xz");
    archive::copy_member(
        &archive,
        "files/files.tar.xz",
        &compressed,
        archive::MAX_ARCHIVE,
    )?;
    // Decompress once, with bounded memory and output, before per-member
    // inspection. The tar decoder never writes archive-selected paths.
    let payload = work.join("payload.tar");
    stream(
        Command::new("xz")
            .args(["-dc", "--memlimit-decompress=256MiB"])
            .arg(&compressed),
        &mut fs::File::create(&payload)?,
        1024 * 1024 * 1024,
    )?;
    let members = archive::list(&payload)?;
    let architectures: BTreeSet<_> = members
        .keys()
        .filter_map(|name| name.split('/').next())
        .collect();
    let (architecture, arch) = match architectures.into_iter().collect::<Vec<_>>().as_slice() {
        ["arm64"] => (Architecture::Aarch64, "arm64"),
        ["x86_64"] => (Architecture::X86_64, "x86_64"),
        _ => return Err("payload must contain exactly one supported 64-bit architecture".into()),
    };
    let prefix = format!("{arch}/{SDK}/system/");
    archive::ensure_payload(&members, &prefix)?;
    let tree = work.join("selected");
    fs::create_dir(&tree)?;
    let mut apks = vec![];
    let mut packages = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut included = BTreeSet::new();
    for (package, path) in CORE_APPS
        .into_iter()
        .chain(SYNC_APPS.into_iter().filter(|_| sync))
    {
        let member = format!("{prefix}{path}");
        if members.get(&member) != Some(&false) {
            return Err(format!("missing APK: {member}").into());
        }
        let destination = tree.join(path);
        archive::copy_member(&payload, &member, &destination, archive::MAX_ARCHIVE)?;
        let (apk, requested) =
            inspect_apk(&destination, package, path, architecture, aapt2, apksigner)?;
        files.insert(path.to_owned(), apk.digest.clone());
        packages.insert(
            package.to_owned(),
            (path.split('/').next().unwrap().to_owned(), requested),
        );
        apks.push(apk);
        included.insert(member);
    }
    for (member, directory) in &members {
        if *directory {
            continue;
        }
        let relative = member
            .strip_prefix(&prefix)
            .ok_or("unexpected payload path")?;
        let Some((partition, rest)) = relative.split_once('/') else {
            continue;
        };
        if !["product", "system_ext"].contains(&partition)
            || !rest.ends_with(".xml")
            || ![
                "etc/permissions/",
                "etc/sysconfig/",
                "etc/default-permissions/",
            ]
            .iter()
            .any(|p| rest.starts_with(p))
        {
            continue;
        }
        let input = archive::member(&payload, member, 1024 * 1024)?;
        if let Some(bytes) =
            xml::select(&input, partition, &packages).map_err(|e| format!("{member}: {e}"))?
        {
            // Namespacing avoids accidental replacement of base XML files.
            let relative_path = Path::new(relative);
            let name = relative_path
                .file_name()
                .ok_or("XML has no filename")?
                .to_str()
                .ok_or("invalid XML name")?;
            let output = relative_path
                .parent()
                .unwrap()
                .join(format!("droidloom-gapps-{name}"));
            let output = output.to_str().ok_or("invalid output path")?.to_owned();
            write(&tree.join(&output), &bytes)?;
            files.insert(
                output,
                FileDigest::read(
                    &tree.join(
                        relative_path
                            .parent()
                            .unwrap()
                            .join(format!("droidloom-gapps-{name}")),
                    ),
                )?,
            );
            included.insert(member.clone());
        }
    }
    let license = archive::member(&archive, "LICENSE", 65_536)?;
    let excluded = members
        .into_iter()
        .filter(|(name, dir)| !dir && !included.contains(name))
        .map(|(name, _)| name)
        .collect();
    Ok(Imported {
        architecture,
        version,
        archive: digest,
        apks,
        files,
        tree,
        license,
        excluded,
    })
}

fn quoted(line: &str, key: &str) -> Option<String> {
    let (_, rest) = line.split_once(&format!("{key}='"))?;
    Some(rest.split_once('\'')?.0.to_owned())
}

fn minimum_sdk(badging: &str) -> Result<u32> {
    // aapt2 renamed sdkVersion to minSdkVersion in Android 17. If uses-sdk
    // appears more than once, Android consumes its last declaration.
    let value = badging
        .lines()
        .filter_map(|line| {
            line.strip_prefix("minSdkVersion:'")
                .or_else(|| line.strip_prefix("sdkVersion:'"))
        })
        .next_back();
    Ok(value
        .map(|v| v.strip_suffix('\'').ok_or("malformed minimum SDK"))
        .transpose()?
        .unwrap_or("1")
        .parse()?)
}

fn inspect_apk(
    path: &Path,
    package: &str,
    relative: &str,
    architecture: Architecture,
    aapt2: &Path,
    apksigner: &Path,
) -> Result<(Apk, BTreeSet<String>)> {
    let badging = text(Command::new(aapt2).args(["dump", "badging"]).arg(path))?;
    let header = badging
        .lines()
        .find(|line| line.starts_with("package:"))
        .ok_or("APK has no package metadata")?;
    if quoted(header, "name").as_deref() != Some(package) || quoted(header, "split").is_some() {
        return Err(format!("unexpected package name or split APK: {relative}").into());
    }
    let version_code = quoted(header, "versionCode")
        .ok_or("missing APK version")?
        .parse()?;
    // An omitted uses-sdk/minSdkVersion defaults to API 1 in Android.
    let min_sdk = minimum_sdk(&badging)?;
    if min_sdk > SDK {
        return Err(format!("{package} requires API {min_sdk}").into());
    }
    let native_abis: Vec<String> = badging
        .lines()
        .find_map(|line| line.strip_prefix("native-code:"))
        .map(|line| {
            line.split_whitespace()
                .map(|s| s.trim_matches('\'').to_owned())
                .collect()
        })
        .unwrap_or_default();
    let abi = match architecture {
        Architecture::Aarch64 => "arm64-v8a",
        Architecture::X86_64 => "x86_64",
    };
    if !native_abis.is_empty() && !native_abis.iter().any(|s| s == abi) {
        return Err(format!("{package} has no {abi} native code").into());
    }
    let verification = text(
        Command::new(apksigner)
            .env("JAVA_TOOL_OPTIONS", "-XX:ActiveProcessorCount=1")
            .args([
                "verify",
                "--print-certs",
                "--min-sdk-version",
                "37",
                "--max-sdk-version",
                "37",
            ])
            .arg(path),
    )?;
    let signer_sha256: Vec<String> = verification
        .lines()
        .filter_map(|line| line.split_once(" certificate SHA-256 digest: "))
        .filter(|(prefix, _)| prefix.starts_with("Signer #"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
        .collect();
    if signer_sha256.is_empty()
        || signer_sha256
            .iter()
            .any(|s| !droidloom_contracts::gapps::is_sha256(s))
    {
        return Err("apksigner did not return verified certificate digests".into());
    }
    let requested = badging
        .lines()
        .filter(|line| {
            line.starts_with("uses-permission:") || line.starts_with("uses-permission-sdk-23:")
        })
        .filter_map(|line| quoted(line, "name"))
        .collect();
    Ok((
        Apk {
            package: package.into(),
            path: relative.into(),
            version_code,
            min_sdk,
            native_abis,
            signer_sha256,
            digest: FileDigest::read(path)?,
        },
        requested,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supports_old_and_current_aapt_sdk_fields_without_accepting_codenames() {
        assert_eq!(minimum_sdk("sdkVersion:'35'\n").unwrap(), 35);
        assert_eq!(minimum_sdk("minSdkVersion:'37'\n").unwrap(), 37);
        assert_eq!(
            minimum_sdk("minSdkVersion:'35'\nminSdkVersion:'37'\n").unwrap(),
            37
        );
        assert_eq!(minimum_sdk("package: name='example'\n").unwrap(), 1);
        assert!(minimum_sdk("minSdkVersion:'Future'\n").is_err());
        assert!(minimum_sdk("minSdkVersion:'37\n").is_err());
    }
}
