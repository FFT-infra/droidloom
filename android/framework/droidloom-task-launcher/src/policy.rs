//! Cache successful display policy for one user and `system_server` lifetime.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::{LaunchError, configure_cell_display_policy};

pub(super) fn configure_once(
    android_command: &Path,
    user: u32,
    cache_directory: &Path,
    proc_directory: &Path,
) -> Result<bool, LaunchError> {
    // Cache failure only costs the optimization. Never skip configuration if
    // its owner, contents, or framework lifetime cannot be established.
    let cache = PolicyCache::open(cache_directory, proc_directory, user).ok();
    if cache.as_ref().is_some_and(|cache| cache.prepared) {
        return Ok(true);
    }
    configure_cell_display_policy(android_command, user)?;
    if let Some(cache) = cache {
        let _ = cache.commit(proc_directory);
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameworkIdentity {
    pid: u32,
    start_ticks: u64,
}

impl FrameworkIdentity {
    fn read(proc_directory: &Path, pid: u32) -> Option<Self> {
        let stat = fs::read_to_string(proc_directory.join(pid.to_string()).join("stat")).ok()?;
        let (process, fields) = stat.rsplit_once(") ")?;
        if process != format!("{pid} (system_server") {
            return None;
        }
        let mut fields = fields.split_whitespace();
        if matches!(fields.next()?, "Z" | "X" | "x") {
            return None;
        }
        // The suffix starts at field 3 (state); starttime is field 22.
        let start_ticks = fields.nth(18)?.parse().ok()?;
        Some(Self { pid, start_ticks })
    }

    fn discover(proc_directory: &Path) -> io::Result<Self> {
        let mut found = None;
        for entry in fs::read_dir(proc_directory)? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            if let Some(identity) = Self::read(proc_directory, pid) {
                if found.replace(identity).is_some() {
                    return Err(io::Error::other("ambiguous system_server identity"));
                }
            }
        }
        found.ok_or_else(|| io::Error::other("system_server identity unavailable"))
    }

    fn decode(record: &str) -> Option<Self> {
        let mut fields = record.split_whitespace();
        let identity = Self {
            pid: fields.next()?.parse().ok()?,
            start_ticks: fields.next()?.parse().ok()?,
        };
        fields.next().is_none().then_some(identity)
    }
}

struct PolicyCache {
    path: PathBuf,
    framework: FrameworkIdentity,
    prepared: bool,
}

impl PolicyCache {
    fn open(directory: &Path, proc_directory: &Path, user: u32) -> io::Result<Self> {
        match DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(directory)?;
        let owner = fs::metadata("/proc/self")?.uid();
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::other("untrusted launch policy cache directory"));
        }
        // /dev is private tmpfs for this Android cell. Bump the version when
        // changing policy; Android user IDs and framework restarts never share
        // successful setup, including when a PID is recycled.
        let path = directory.join(format!("v2-user-{user}"));
        let cached = fs::read_to_string(&path)
            .ok()
            .and_then(|record| FrameworkIdentity::decode(&record))
            .filter(|identity| {
                FrameworkIdentity::read(proc_directory, identity.pid) == Some(*identity)
            });
        Ok(Self {
            path,
            framework: cached.map_or_else(|| FrameworkIdentity::discover(proc_directory), Ok)?,
            prepared: cached.is_some(),
        })
    }

    fn commit(&self, proc_directory: &Path) -> io::Result<()> {
        if FrameworkIdentity::read(proc_directory, self.framework.pid) != Some(self.framework) {
            return Err(io::Error::other("system_server changed during setup"));
        }
        // Publish only a complete successful transaction. Parallel launchers
        // may repeat idempotent setup, but cannot observe partial preparation.
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = writeln!(
            file,
            "{} {}",
            self.framework.pid, self.framework.start_ticks
        )
        .and_then(|()| fs::rename(&temporary, &self.path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn framework(proc_directory: &Path, pid: u32, start_ticks: u64) {
        let directory = proc_directory.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        let mut fields = vec!["S".to_owned()];
        fields.extend((4..22).map(|_| "0".to_owned()));
        fields.push(start_ticks.to_string());
        fs::write(
            directory.join("stat"),
            format!("{pid} (system_server) {}\n", fields.join(" ")),
        )
        .unwrap();
    }

    fn command(directory: &Path) -> PathBuf {
        let path = directory.join("cmd");
        fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0}.calls\"\nif [ -e \"${0}.fail\" ] && [ \"$1\" = lock_settings ]; then exit 1; fi\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn warm_setup_skips_commands_and_new_users_or_frameworks_prepare_again() {
        let directory = tempfile::tempdir().unwrap();
        let proc_directory = directory.path().join("proc");
        let cache = directory.path().join("cache");
        framework(&proc_directory, 123, 456);
        let command = command(directory.path());
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        assert!(configure_once(&command, 0, &cache, &proc_directory).unwrap());
        assert_eq!(
            fs::read_to_string(command.with_extension("calls"))
                .unwrap()
                .lines()
                .count(),
            6
        );
        assert!(!configure_once(&command, 10, &cache, &proc_directory).unwrap());
        assert!(configure_once(&command, 0, &cache, &proc_directory).unwrap());
        // Same PID, different start time: never reuse the previous setup.
        framework(&proc_directory, 123, 789);
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        assert_eq!(
            fs::read_to_string(command.with_extension("calls"))
                .unwrap()
                .lines()
                .count(),
            18
        );
    }

    #[test]
    fn failed_setup_is_retried_and_never_marks_the_user_prepared() {
        let directory = tempfile::tempdir().unwrap();
        let proc_directory = directory.path().join("proc");
        let cache = directory.path().join("cache");
        framework(&proc_directory, 123, 456);
        let command = command(directory.path());
        fs::write(command.with_extension("fail"), "").unwrap();
        assert!(configure_once(&command, 0, &cache, &proc_directory).is_err());
        assert!(!cache.join("v2-user-0").exists());
        fs::remove_file(command.with_extension("fail")).unwrap();
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        assert!(configure_once(&command, 0, &cache, &proc_directory).unwrap());
    }

    #[test]
    fn changed_framework_during_setup_cannot_publish_success() {
        let directory = tempfile::tempdir().unwrap();
        let proc_directory = directory.path().join("proc");
        let cache_directory = directory.path().join("cache");
        framework(&proc_directory, 123, 456);
        let cache = PolicyCache::open(&cache_directory, &proc_directory, 0).unwrap();
        framework(&proc_directory, 123, 789);
        assert!(cache.commit(&proc_directory).is_err());
        assert!(!cache.path.exists());
    }

    #[test]
    fn unavailable_or_untrusted_cache_falls_back_to_full_setup() {
        let directory = tempfile::tempdir().unwrap();
        let proc_directory = directory.path().join("proc");
        let cache = directory.path().join("cache");
        framework(&proc_directory, 123, 456);
        let command = command(directory.path());
        fs::create_dir(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        assert!(!cache.join("v2-user-0").exists());
        let linked = directory.path().join("linked-cache");
        symlink(&cache, &linked).unwrap();
        assert!(!configure_once(&command, 0, &linked, &proc_directory).unwrap());
        assert!(
            !configure_once(
                &command,
                0,
                &directory.path().join("missing/cache"),
                &proc_directory
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(command.with_extension("calls"))
                .unwrap()
                .lines()
                .count(),
            18
        );
    }

    #[test]
    fn malformed_cache_or_missing_framework_cannot_skip_setup() {
        let directory = tempfile::tempdir().unwrap();
        let proc_directory = directory.path().join("proc");
        let cache = directory.path().join("cache");
        framework(&proc_directory, 123, 456);
        let command = command(directory.path());
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        fs::write(cache.join("v2-user-0"), "123 456 extra").unwrap();
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
        fs::remove_file(proc_directory.join("123/stat")).unwrap();
        assert!(!configure_once(&command, 0, &cache, &proc_directory).unwrap());
    }
}
