//! Package setup and pacman lifecycle integration. No shell interpreter is used.
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const BASE: &str = "/etc/droidloom";
const SPEC: &str = "/etc/droidloom/cell.json";
const STATE: &str = "/etc/droidloom/package-state.json";
const ENVIRONMENT: &str = "/etc/droidloom/runtime.env";
const RULE: &str = "/etc/polkit-1/rules.d/49-droidloom.rules";
const HELPER: &str = "/usr/lib/droidloom/droidloom-package-helper";
const RECIPE: &str = "/usr/share/droidloom/cell-template.json";
const VERSION: &str = "/usr/share/droidloom/package.json";

#[derive(Parser)]
#[command(
    version,
    about = "Configure and maintain a pacman-managed Droidloom installation"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Perform missing setup as the calling desktop user, explaining authentication.
    Prepare,
    #[command(hide = true)]
    Setup {
        #[arg(long)]
        uid: u32,
        #[arg(long)]
        data_home: Option<PathBuf>,
        #[arg(long)]
        state_home: Option<PathBuf>,
    },
    #[command(hide = true)]
    BeforeUpgrade,
    #[command(hide = true)]
    BeforeRemove,
    #[command(hide = true)]
    AfterInstall,
}
#[derive(Clone, Deserialize, Serialize)]
struct SetupState {
    uid: u32,
    version: Value,
    data_home: PathBuf,
    state_home: PathBuf,
}
struct User {
    name: String,
    home: PathBuf,
}
fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn optional_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed: {status}").into());
    }
    Ok(())
}
fn user(uid: u32) -> Result<User> {
    if uid == 0 {
        return Err("Droidloom's desktop owner must be an ordinary user".into());
    }
    let record = Command::new("/usr/bin/getent")
        .args(["passwd", &uid.to_string()])
        .output()?;
    if !record.status.success() {
        return Err(format!("no desktop user exists for UID {uid}").into());
    }
    let record = String::from_utf8(record.stdout)?;
    let fields: Vec<_> = record.trim_end().split(':').collect();
    if fields.len() != 7 || fields[2].parse::<u32>()? != uid || !Path::new(fields[5]).is_absolute()
    {
        return Err("invalid desktop user record".into());
    }
    Ok(User {
        name: fields[0].into(),
        home: fields[5].into(),
    })
}
fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("this operation writes root-owned Droidloom configuration or controls its system service; run through pacman or the authenticated setup operation".into());
    }
    Ok(())
}
fn root_directory(path: &Path, mode: u32) -> Result<()> {
    if path == Path::new("/") {
        return Ok(());
    }
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(
            "administrator-owned directory must be an absolute path without parent traversal"
                .into(),
        );
    }
    if let Some(parent) = path.parent() {
        root_directory(parent, 0o755)?;
    }
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => Ok(()),
        Ok(_) => Err(format!(
            "refusing non-directory, symlink, or writable/non-root-owned setup path {}",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
fn atomic_write(path: &Path, value: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or("configuration path has no parent")?;
    root_directory(parent, 0o755)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "refusing to replace unsafe configuration {}",
                path.display()
            )
            .into());
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(value)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn render_node(sys: &Path) -> Result<String> {
    let mut nodes = fs::read_dir(sys)?.collect::<std::io::Result<Vec<_>>>()?;
    nodes.sort_by_key(|n| n.file_name());
    for node in nodes {
        let name = node.file_name().to_string_lossy().into_owned();
        if !name
            .strip_prefix("renderD")
            .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        if let Ok(driver) = fs::canonicalize(node.path().join("device/driver")) {
            if driver
                .file_name()
                .is_some_and(|d| d == "amdgpu" || d == "i915" || d == "xe")
            {
                return Ok(format!("/dev/dri/{name}"));
            }
        }
    }
    Err("This Droidloom package needs an AMD or Intel render node. No matching driver was found; configuration was not changed.".into())
}
fn configuration(
    mut recipe: Value,
    previous: Option<&Value>,
    uid: u32,
    render: &str,
) -> Result<Value> {
    if uid == 0 {
        return Err("desktop UID must be nonzero".into());
    }
    recipe["host_uid"] = json!(uid);
    recipe["render_node"] = json!(render);
    recipe["data_dir"] = json!(format!("/var/lib/droidloom/users/{uid}/data"));
    recipe["runtime_dir"] = json!(format!("/run/droidloom/cells/u{uid}"));
    recipe["denial_socket"] = json!(format!("/run/user/{uid}/droidloom/native-bridge.sock"));
    if let Some(previous) = previous {
        if previous["host_uid"].as_u64() != Some(u64::from(uid)) {
            return Err("this Droidloom installation belongs to another desktop user; existing data and configuration were preserved".into());
        }
        for key in [
            "data_dir",
            "gapps_dir",
            "shared_storage_directories",
            "subordinate_uids",
            "subordinate_gids",
        ] {
            if let Some(value) = previous.get(key) {
                recipe[key] = value.clone();
            }
        }
    }
    let data = Path::new(
        recipe["data_dir"]
            .as_str()
            .ok_or("missing Android data path")?,
    );
    if !data.starts_with("/var/lib/droidloom")
        || data.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err("Android data must remain below /var/lib/droidloom".into());
    }
    let spec: droidloom_supervisor::CellSpec = serde_json::from_value(recipe.clone())?;
    spec.validate()?;
    Ok(recipe)
}
fn prepare() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let account = user(uid)?;
    if unsafe { libc::geteuid() } == 0 {
        return Err("run droidloomctl start as your desktop user, without sudo".into());
    }
    let previous = optional_json(Path::new(SPEC))?;
    if previous
        .as_ref()
        .is_some_and(|s| s["host_uid"].as_u64() != Some(u64::from(uid)))
    {
        return Err("Droidloom is configured for a different desktop user".into());
    }
    let version = read_json(Path::new(VERSION))?;
    let state: Option<SetupState> = optional_json(Path::new(STATE))?
        .map(serde_json::from_value)
        .transpose()?;
    if state
        .as_ref()
        .is_some_and(|s| s.uid == uid && s.version == version)
        && previous.is_some()
        && Path::new(ENVIRONMENT).is_file()
    {
        // Polkit's rules directory is intentionally not traversable by an
        // ordinary user. The root-owned state is published after the rule.
        return Ok(());
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| account.home.join(".local/share"));
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| account.home.join(".local/state"));
    let data = previous
        .as_ref()
        .and_then(|s| s["data_dir"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/var/lib/droidloom/users/{uid}/data"));
    eprintln!(
        "Administrator authentication is needed to configure Droidloom for {} (UID {uid}).",
        account.name
    );
    eprintln!(
        "The installed helper will write {SPEC}, {ENVIRONMENT}, {STATE}, and {RULE}. These files are administrator-owned."
    );
    eprintln!(
        "It will create missing Android data.img and metadata.img regular files in {data}, initialize only new files as ext4, and preserve existing Android data. It authorizes only start/stop of droidloomd.service for this user."
    );
    eprintln!(
        "That system service needs root to create Android namespaces, mount the image files, and configure Droidloom's network connection."
    );
    run(Command::new("/usr/bin/pkexec")
        .arg(HELPER)
        .args(["setup", "--uid"])
        .arg(uid.to_string())
        .arg("--data-home")
        .arg(data_home)
        .arg("--state-home")
        .arg(state_home))
}
fn setup(
    uid: u32,
    data_home: Option<PathBuf>,
    state_home: Option<PathBuf>,
    hook: bool,
) -> Result<()> {
    require_root()?;
    if !hook {
        if let Ok(invoker) = std::env::var("PKEXEC_UID").or_else(|_| std::env::var("SUDO_UID")) {
            if invoker.parse::<u32>()? != uid {
                return Err("setup owner differs from the authenticated caller".into());
            }
        }
    }
    let account = user(uid)?;
    root_directory(Path::new(BASE), 0o755)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(Path::new(BASE).join("setup.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let previous = optional_json(Path::new(SPEC))?;
    let renderer = if hook {
        previous
            .as_ref()
            .and_then(|p| p["render_node"].as_str())
            .ok_or("existing setup has no render node")?
            .to_owned()
    } else {
        render_node(Path::new("/sys/class/drm"))?
    };
    let spec = configuration(
        read_json(Path::new(RECIPE))?,
        previous.as_ref(),
        uid,
        &renderer,
    )?;
    let data = Path::new(spec["data_dir"].as_str().unwrap());
    root_directory(data, 0o700)?;
    for (name, bytes) in [
        ("data.img", 12_u64 * 1024 * 1024 * 1024),
        ("metadata.img", 128_u64 * 1024 * 1024),
    ] {
        let path = data.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0 => continue,
            Ok(_) => {
                return Err(format!("refusing unsafe Android data file {}", path.display()).into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let file = tempfile::NamedTempFile::new_in(data)?;
        file.as_file().set_len(bytes)?;
        run(Command::new("/usr/bin/mkfs.ext4")
            .args(["-F", "-q"])
            .arg(file.path()))?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&path)?;
    }
    let old_state: Option<SetupState> = optional_json(Path::new(STATE))?
        .map(serde_json::from_value)
        .transpose()?;
    let state = SetupState {
        uid,
        version: read_json(Path::new(VERSION))?,
        data_home: data_home
            .or_else(|| old_state.as_ref().map(|s| s.data_home.clone()))
            .unwrap_or_else(|| account.home.join(".local/share")),
        state_home: state_home
            .or_else(|| old_state.as_ref().map(|s| s.state_home.clone()))
            .unwrap_or_else(|| account.home.join(".local/state")),
    };
    if !state.data_home.is_absolute() || !state.state_home.is_absolute() {
        return Err("XDG paths must be absolute".into());
    }
    let username = serde_json::to_string(&account.name)?;
    let rule = format!(
        "// Generated for Droidloom's configured desktop user.\npolkit.addRule(function(action, subject) {{\n  if (action.id === \"org.freedesktop.systemd1.manage-units\" && subject.user === {username} && action.lookup(\"unit\") === \"droidloomd.service\" && (action.lookup(\"verb\") === \"start\" || action.lookup(\"verb\") === \"stop\")) return polkit.Result.YES;\n}});\n"
    );
    atomic_write(Path::new(SPEC), &serde_json::to_vec_pretty(&spec)?, 0o644)?;
    atomic_write(Path::new(ENVIRONMENT), format!("DROIDLOOM_HOST_PEER_PID=0\nDROIDLOOM_HOST_PEER_UID=1000\nDROIDLOOM_HOST_PEER_GID=1000\nDROIDLOOM_RENDER_NODE={renderer}\n").as_bytes(), 0o644)?;
    atomic_write(Path::new(RULE), rule.as_bytes(), 0o644)?;
    atomic_write(Path::new(STATE), &serde_json::to_vec_pretty(&state)?, 0o644)?;
    println!(
        "Droidloom is configured for {}. Automatic startup remains disabled.",
        account.name
    );
    Ok(())
}
fn user_command(uid: u32, executable: &str) -> Result<Command> {
    let account = user(uid)?;
    let mut command = Command::new("/usr/bin/runuser");
    command
        .args(["-u", &account.name, "--", "/usr/bin/env"])
        .arg(format!("XDG_RUNTIME_DIR=/run/user/{uid}"))
        .arg(format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"
        ))
        .arg(executable);
    Ok(command)
}
fn stop_runtime(spec: Option<&Value>) -> Result<()> {
    if !Path::new("/run/systemd/system").exists() {
        return Ok(());
    }
    if let Some(uid) = spec
        .and_then(|s| s["host_uid"].as_u64())
        .and_then(|v| u32::try_from(v).ok())
    {
        if Path::new(&format!("/run/user/{uid}/bus")).exists() {
            run(user_command(uid, "/usr/bin/systemctl")?.args([
                "--user",
                "stop",
                "droidloom.service",
                "droidloom-applications.service",
            ]))?;
        }
    }
    run(Command::new("/usr/bin/systemctl").args(["stop", "droidloomd.service"]))?;
    let result = Command::new("/usr/bin/systemctl")
        .args([
            "show",
            "--property=ActiveState",
            "--value",
            "droidloomd.service",
        ])
        .output()?;
    let state = String::from_utf8(result.stdout)?;
    if !result.status.success() || !matches!(state.trim(), "inactive" | "failed") {
        return Err("Droidloom has not stopped; refusing to replace mounted Android images".into());
    }
    Ok(())
}
fn before(remove: bool) -> Result<()> {
    require_root()?;
    let spec = optional_json(Path::new(SPEC))?;
    stop_runtime(spec.as_ref())?;
    if remove {
        if let Some(value) = optional_json(Path::new(STATE))? {
            let state: SetupState = serde_json::from_value(value)?;
            let account = user(state.uid)?;
            run(Command::new("/usr/bin/runuser")
                .args(["-u", &account.name, "--", "/usr/bin/env"])
                .arg(format!("XDG_DATA_HOME={}", state.data_home.display()))
                .arg(format!("XDG_STATE_HOME={}", state.state_home.display()))
                .args(["/usr/bin/droidloom-applications", "--remove-managed"]))?;
        }
        for path in [RULE, ENVIRONMENT, STATE] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        println!(
            "Droidloom stopped. Android application data and personal configuration were retained."
        );
    }
    Ok(())
}
fn after_install() -> Result<()> {
    require_root()?;
    if let Some(spec) = optional_json(Path::new(SPEC))? {
        let uid = u32::try_from(
            spec["host_uid"]
                .as_u64()
                .ok_or("invalid existing desktop owner")?,
        )?;
        setup(uid, None, None, true)?;
    }
    println!("Droidloom is installed and stopped. Run droidloomctl start as your desktop user.");
    println!(
        "First start may request administrator authentication to create /etc/droidloom configuration, the service authorization rule, and Android data files under /var/lib/droidloom. Builds and subsequent starts do not need sudo."
    );
    Ok(())
}
fn execute() -> Result<()> {
    match Cli::parse().action {
        Action::Prepare => prepare(),
        Action::Setup {
            uid,
            data_home,
            state_home,
        } => setup(uid, data_home, state_home, false),
        Action::BeforeUpgrade => before(false),
        Action::BeforeRemove => before(true),
        Action::AfterInstall => after_install(),
    }
}
fn main() {
    if let Err(error) = execute() {
        eprintln!("Droidloom package setup failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recipe() -> Value {
        let mut recipe: Value = serde_json::from_str(include_str!(
            "../../../packaging/cell-spec-x86_64-u1000.json"
        ))
        .unwrap();
        recipe["image_dir"] = json!("/usr/lib/droidloom/images");
        recipe["vendor_image"] = json!("/usr/lib/droidloom/images/images/vendor.raw.img");
        recipe["shared_storage_directories"] = json!([]);
        recipe
    }
    #[test]
    fn setup_uses_target_user_and_retains_personal_state_during_upgrade() {
        let mut previous = configuration(recipe(), None, 1234, "/dev/dri/renderD129").unwrap();
        assert_eq!(
            previous["denial_socket"],
            "/run/user/1234/droidloom/native-bridge.sock"
        );
        assert_eq!(previous["runtime_dir"], "/run/droidloom/cells/u1234");
        assert_eq!(previous["data_dir"], "/var/lib/droidloom/users/1234/data");
        previous["data_dir"] = json!("/var/lib/droidloom/preserved-data");
        previous["gapps_dir"] = json!("/usr/lib/droidloom/addons/gapps");
        previous["android_file_overrides"] = json!([]);
        let updated =
            configuration(recipe(), Some(&previous), 1234, "/dev/dri/renderD128").unwrap();
        assert_eq!(updated["data_dir"], previous["data_dir"]);
        assert_eq!(updated["gapps_dir"], previous["gapps_dir"]);
        assert!(
            !updated["android_file_overrides"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(configuration(recipe(), Some(&previous), 1000, "/dev/dri/renderD128").is_err());
    }
    #[test]
    fn setup_rejects_root_and_data_path_escape() {
        assert!(configuration(recipe(), None, 0, "/dev/dri/renderD128").is_err());
        let mut previous = configuration(recipe(), None, 1000, "/dev/dri/renderD128").unwrap();
        previous["data_dir"] = json!("/var/lib/droidloom/../../../home/test");
        assert!(configuration(recipe(), Some(&previous), 1000, "/dev/dri/renderD128").is_err());
    }
}
