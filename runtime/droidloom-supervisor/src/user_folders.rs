//! The desktop user's own Downloads, Pictures, Documents, Music and Videos,
//! shown to Android as its Download, Pictures, Documents, Music and Movies.
//!
//! The folders are found at each start from the user's home and
//! `~/.config/user-dirs.dirs`, so a renamed or newly made folder follows. A
//! folder is shared only while it is a real directory owned by the user and
//! the user's primary group; Android keeps its own folder otherwise.

use std::ffi::CStr;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::{CellSpec, SharedStorageDirectory};

/// XDG key, its usual folder below the home, and Android's folder.
const FOLDERS: [(&str, &str, &str); 5] = [
    ("XDG_DOWNLOAD_DIR", "Downloads", "Download"),
    ("XDG_PICTURES_DIR", "Pictures", "Pictures"),
    ("XDG_DOCUMENTS_DIR", "Documents", "Documents"),
    ("XDG_MUSIC_DIR", "Music", "Music"),
    ("XDG_VIDEOS_DIR", "Videos", "Movies"),
];
const MAX_USER_DIRS_BYTES: u64 = 16 * 1024;

/// The user's home and primary group.
fn account(uid: u32) -> Option<(PathBuf, u32)> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    let home = unsafe { CStr::from_ptr(entry.pw_dir) }.to_str().ok()?;
    let home = PathBuf::from(home);
    (home.is_absolute() && home != Path::new("/")).then_some((home, entry.pw_gid))
}

/// The folder `key` names in `user-dirs.dirs`, below `home`.
fn configured(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
    let value = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')
            .map(str::to_owned)
    })?;
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // A folder set to the home itself is how xdg-user-dirs turns one off.
    (path.starts_with(home)
        && path != home
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))))
    .then_some(path)
}

fn user_dirs(home: &Path, uid: u32) -> String {
    let path = home.join(".config/user-dirs.dirs");
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.is_file()
                && metadata.uid() == uid
                && metadata.len() <= MAX_USER_DIRS_BYTES =>
        {
            fs::read_to_string(path).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// The user's folders that can be shared now, by Android name, leaving out
/// the names `spec` already shares explicitly.
pub(crate) fn resolve(spec: &CellSpec) -> Vec<SharedStorageDirectory> {
    if !spec.share_user_folders {
        return Vec::new();
    }
    let Some((home, gid)) = account(spec.host_uid) else {
        return Vec::new();
    };
    // Every shared folder maps to Android's media group through one host group.
    if spec
        .shared_storage_directories
        .first()
        .is_some_and(|first| first.host_gid != gid)
    {
        return Vec::new();
    }
    let text = user_dirs(&home, spec.host_uid);
    FOLDERS
        .iter()
        .filter(|(_, _, android)| {
            !spec
                .shared_storage_directories
                .iter()
                .any(|directory| directory.android_name == *android)
        })
        .filter_map(|(key, usual, android)| {
            let source = configured(&text, key, &home).unwrap_or_else(|| home.join(usual));
            let metadata = fs::symlink_metadata(&source).ok()?;
            (metadata.is_dir() && metadata.uid() == spec.host_uid && metadata.gid() == gid).then(
                || SharedStorageDirectory {
                    source,
                    android_name: (*android).to_owned(),
                    host_gid: gid,
                },
            )
        })
        .collect()
}

/// Gives each user folder the setgid bit, so files Android makes there keep
/// the user's group.
pub(crate) fn prepare(spec: &CellSpec) -> io::Result<()> {
    for folder in resolve(spec) {
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&folder.source)?;
        let metadata = directory.metadata()?;
        if metadata.uid() != spec.host_uid || metadata.gid() != folder.host_gid {
            continue;
        }
        if metadata.mode() & 0o2000 == 0 {
            directory.set_permissions(fs::Permissions::from_mode(
                (metadata.mode() & 0o7777) | 0o2000,
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dirs_are_read_below_home_only() {
        let home = Path::new("/home/test");
        let text = "# xdg\nXDG_DOWNLOAD_DIR=\"$HOME/Hämtningar\"\nXDG_MUSIC_DIR=\"$HOME/\"\n\
                    XDG_VIDEOS_DIR=\"/srv/videos\"\nXDG_PICTURES_DIR=\"$HOME/../other\"\n";
        assert_eq!(
            configured(text, "XDG_DOWNLOAD_DIR", home),
            Some(PathBuf::from("/home/test/Hämtningar"))
        );
        assert_eq!(configured(text, "XDG_MUSIC_DIR", home), None);
        assert_eq!(configured(text, "XDG_VIDEOS_DIR", home), None);
        assert_eq!(configured(text, "XDG_PICTURES_DIR", home), None);
        assert_eq!(configured(text, "XDG_DOCUMENTS_DIR", home), None);
    }
}
