//! Persistent, compositor-independent initial geometry for Android windows.
//!
//! XDG toplevels may receive a size-less initial configure. Ordinary clients
//! answer that request with their own preferred size; this crate gives the
//! Droidloom presenter the same explicit policy without coupling it to Denial.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

const SCHEMA: u32 = 1;
const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;
const MAX_APPLICATIONS: usize = 4096;
const MAX_DIMENSION: u32 = 16_384;

/// Portable default used only when neither XDG bounds nor output geometry is
/// available. It is a preference, never an eagerly published buffer size.
pub const PORTABLE_WINDOWED_DEFAULT: LogicalSize = LogicalSize {
    width: 480,
    height: 800,
};

/// Explicit runtime form factor, selected when starting Droidloom.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionMode {
    /// Prefer ordinary, remembered desktop windows.
    #[default]
    Desktop,
    /// Prefer the complete available mobile work area.
    Mobile,
}

impl SessionMode {
    /// Value accepted by the startup flag and service environment.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }
}

impl std::fmt::Display for SessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionMode {
    type Err = WindowPolicyError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "desktop" => Ok(Self::Desktop),
            "mobile" => Ok(Self::Mobile),
            _ => Err(WindowPolicyError::Invalid(
                "mode must be desktop or mobile".into(),
            )),
        }
    }
}

/// A non-empty logical Wayland extent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalSize {
    /// Width in Wayland logical coordinates.
    pub width: u32,
    /// Height in Wayland logical coordinates.
    pub height: u32,
}

impl LogicalSize {
    /// Construct and validate a logical extent.
    ///
    /// # Errors
    ///
    /// Rejects zero or unreasonably large dimensions.
    pub fn new(width: u32, height: u32) -> Result<Self, WindowPolicyError> {
        let value = Self { width, height };
        value.validate()?;
        Ok(value)
    }

    fn validate(self) -> Result<(), WindowPolicyError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            return Err(WindowPolicyError::Invalid(format!(
                "window size {}x{} is outside 1..={MAX_DIMENSION}",
                self.width, self.height
            )));
        }
        Ok(())
    }

    fn clamped_to(self, bounds: Option<Self>) -> Self {
        bounds.map_or(self, |bounds| Self {
            width: self.width.min(bounds.width),
            height: self.height.min(bounds.height),
        })
    }
}

/// How a size-less initial XDG configure is resolved.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    /// Use the compositor's standard XDG bounds, falling back to the logical
    /// output extent. This is the natural mobile policy.
    FitOutput,
    /// Use an application-like fixed logical preference, clamped to XDG
    /// bounds. This is the natural desktop policy.
    Windowed,
}

/// One persistent launch preference.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPreference {
    /// Size selection mode.
    pub mode: LaunchMode,
    /// Required only for [`LaunchMode::Windowed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<LogicalSize>,
}

impl WindowPreference {
    /// Prefer the complete compositor work area.
    pub const fn fit_output() -> Self {
        Self {
            mode: LaunchMode::FitOutput,
            size: None,
        }
    }

    /// Prefer one fixed logical size.
    pub const fn windowed(size: LogicalSize) -> Self {
        Self {
            mode: LaunchMode::Windowed,
            size: Some(size),
        }
    }

    fn validate(self) -> Result<(), WindowPolicyError> {
        match (self.mode, self.size) {
            (LaunchMode::FitOutput, None) => Ok(()),
            (LaunchMode::Windowed, Some(size)) => size.validate(),
            (LaunchMode::FitOutput, Some(_)) => Err(WindowPolicyError::Invalid(
                "fit_output preference must not contain a fixed size".into(),
            )),
            (LaunchMode::Windowed, None) => Err(WindowPolicyError::Invalid(
                "windowed preference requires a fixed size".into(),
            )),
        }
    }
}

impl Default for WindowPreference {
    fn default() -> Self {
        Self::windowed(PORTABLE_WINDOWED_DEFAULT)
    }
}

/// User-owned configuration and state paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowPolicyPaths {
    /// User-edited launch policy.
    pub configuration: PathBuf,
    /// Automatically remembered final application sizes.
    pub state: PathBuf,
}

impl WindowPolicyPaths {
    /// Resolve standard XDG configuration and state paths.
    ///
    /// Relative overrides are ignored as required by the base-directory
    /// specification.
    ///
    /// # Errors
    ///
    /// Fails if no absolute home/configuration/state roots are available.
    pub fn from_environment(
        config_home: Option<OsString>,
        state_home: Option<OsString>,
        home: Option<OsString>,
    ) -> Result<Self, WindowPolicyError> {
        let home = home.map(PathBuf::from).filter(|path| path.is_absolute());
        let configuration_root = absolute_path(config_home)
            .or_else(|| home.as_ref().map(|path| path.join(".config")))
            .ok_or(WindowPolicyError::MissingHome)?;
        let state_root = absolute_path(state_home)
            .or_else(|| home.as_ref().map(|path| path.join(".local/state")))
            .ok_or(WindowPolicyError::MissingHome)?;
        Ok(Self {
            configuration: configuration_root.join("droidloom/window-policy-v1.json"),
            state: state_root.join("droidloom/desktop-window-sizes-v1.json"),
        })
    }

    /// Construct explicit paths for tests and packaging tools.
    pub const fn new(configuration: PathBuf, state: PathBuf) -> Self {
        Self {
            configuration,
            state,
        }
    }
}

/// Loaded launch policy and remembered application sizes.
#[derive(Debug)]
pub struct WindowPolicyStore {
    paths: Option<WindowPolicyPaths>,
    policy: PolicyDocument,
    state: StateDocument,
    mode: SessionMode,
}

impl WindowPolicyStore {
    /// Load a persistent store.
    ///
    /// Missing files select safe defaults; malformed files are rejected rather
    /// than partially interpreted.
    ///
    /// # Errors
    ///
    /// Returns bounded filesystem, JSON, or validation failures.
    pub fn load(paths: WindowPolicyPaths) -> Result<Self, WindowPolicyError> {
        let policy: PolicyDocument = read_document(&paths.configuration)?.unwrap_or_default();
        let state: StateDocument = read_document(&paths.state)?.unwrap_or_default();
        policy.validate()?;
        state.validate()?;
        Ok(Self {
            paths: Some(paths),
            policy,
            state,
            mode: SessionMode::Desktop,
        })
    }

    /// Create an in-memory store using the portable defaults.
    pub fn ephemeral() -> Self {
        Self {
            paths: None,
            policy: PolicyDocument::default(),
            state: StateDocument::default(),
            mode: SessionMode::Desktop,
        }
    }

    /// Select the session default without rewriting user preferences or size history.
    pub fn with_session_mode(mut self, mode: SessionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Reload only the user-edited policy. This is intended to run once per
    /// application creation, never in a frame or input loop.
    ///
    /// # Errors
    ///
    /// Returns malformed or inaccessible configuration errors while leaving
    /// the previously loaded policy intact.
    pub fn reload_policy(&mut self) -> Result<(), WindowPolicyError> {
        let Some(paths) = self.paths.as_ref() else {
            return Ok(());
        };
        let policy: PolicyDocument = read_document(&paths.configuration)?.unwrap_or_default();
        policy.validate()?;
        self.policy = policy;
        Ok(())
    }

    /// Resolve a size-less or partially sized initial XDG configure.
    ///
    /// Explicit compositor dimensions remain authoritative. Missing axes use
    /// a per-package policy when present. The default fit-output policy always
    /// follows the current compositor bounds, while the default windowed
    /// policy restores the package's last stable size before falling back to
    /// its configured size. The result is constrained to advertised bounds.
    pub fn resolve_initial(
        &self,
        package: &str,
        compositor_width: Option<u32>,
        compositor_height: Option<u32>,
        suggested_bounds: Option<LogicalSize>,
        output_size: Option<LogicalSize>,
    ) -> LogicalSize {
        let bounds = suggested_bounds
            .filter(|size| size.validate().is_ok())
            .or(output_size.filter(|size| size.validate().is_ok()));
        let default = if self.mode == SessionMode::Mobile {
            WindowPreference::fit_output()
        } else {
            self.policy.default
        };
        let base = self.policy.applications.get(package).copied().map_or_else(
            || match default.mode {
                LaunchMode::FitOutput => resolve_preference(default, bounds, output_size),
                LaunchMode::Windowed => self.state.applications.get(package).copied().map_or_else(
                    || resolve_preference(default, bounds, output_size),
                    |remembered| remembered.clamped_to(bounds),
                ),
            },
            |preference| resolve_preference(preference, bounds, output_size),
        );
        LogicalSize {
            width: compositor_width.unwrap_or(base.width),
            height: compositor_height.unwrap_or(base.height),
        }
        .clamped_to(bounds)
    }

    /// Remember a stable, non-interactive compositor size for a package.
    ///
    /// The file is rewritten atomically only when the value changes.
    ///
    /// # Errors
    ///
    /// Rejects malformed package/size inputs and propagates filesystem errors.
    pub fn remember(
        &mut self,
        package: &str,
        size: LogicalSize,
    ) -> Result<bool, WindowPolicyError> {
        validate_package(package)?;
        size.validate()?;
        if self.mode == SessionMode::Mobile {
            return Ok(false);
        }
        if self.state.applications.get(package) == Some(&size) {
            return Ok(false);
        }
        if !self.state.applications.contains_key(package)
            && self.state.applications.len() >= MAX_APPLICATIONS
        {
            return Err(WindowPolicyError::Invalid(format!(
                "window-size state exceeds {MAX_APPLICATIONS} applications"
            )));
        }
        let mut next = self.state.clone();
        next.applications.insert(package.to_owned(), size);
        if let Some(paths) = self.paths.as_ref() {
            write_document(&paths.state, &next)?;
        }
        self.state = next;
        Ok(true)
    }

    /// Persist a default or package-specific launch preference.
    ///
    /// # Errors
    ///
    /// Requires a persistent store and rejects malformed inputs or writes.
    pub fn set_preference(
        &mut self,
        package: Option<&str>,
        preference: WindowPreference,
    ) -> Result<(), WindowPolicyError> {
        preference.validate()?;
        let paths = self
            .paths
            .as_ref()
            .ok_or(WindowPolicyError::NotPersistent)?;
        let mut next = self.policy.clone();
        if let Some(package) = package {
            validate_package(package)?;
            if !next.applications.contains_key(package)
                && next.applications.len() >= MAX_APPLICATIONS
            {
                return Err(WindowPolicyError::Invalid(format!(
                    "window policy exceeds {MAX_APPLICATIONS} applications"
                )));
            }
            next.applications.insert(package.to_owned(), preference);
        } else {
            next.default = preference;
        }
        write_document(&paths.configuration, &next)?;
        self.policy = next;
        Ok(())
    }
}

/// Configuration, state, validation, or serialization failure.
#[derive(Debug, Error)]
pub enum WindowPolicyError {
    /// XDG base directories could not be resolved.
    #[error("HOME and the required XDG base directories are unavailable or relative")]
    MissingHome,
    /// An operation requiring a persistent path was attempted in memory.
    #[error("window policy store has no persistent path")]
    NotPersistent,
    /// Bounded policy data was invalid.
    #[error("invalid Droidloom window policy: {0}")]
    Invalid(String),
    /// Filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted.
        context: &'static str,
        /// Underlying failure.
        source: io::Error,
    },
    /// JSON encoding or decoding failed.
    #[error("Droidloom window policy JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    schema: u32,
    default: WindowPreference,
    #[serde(default)]
    applications: BTreeMap<String, WindowPreference>,
}

impl Default for PolicyDocument {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            default: WindowPreference::default(),
            applications: BTreeMap::new(),
        }
    }
}

impl PolicyDocument {
    fn validate(&self) -> Result<(), WindowPolicyError> {
        validate_schema(self.schema)?;
        self.default.validate()?;
        validate_map(&self.applications, |preference| preference.validate())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateDocument {
    schema: u32,
    #[serde(default)]
    applications: BTreeMap<String, LogicalSize>,
}

impl Default for StateDocument {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            applications: BTreeMap::new(),
        }
    }
}

impl StateDocument {
    fn validate(&self) -> Result<(), WindowPolicyError> {
        validate_schema(self.schema)?;
        validate_map(&self.applications, |size| size.validate())
    }
}

fn resolve_preference(
    preference: WindowPreference,
    bounds: Option<LogicalSize>,
    output_size: Option<LogicalSize>,
) -> LogicalSize {
    match preference.mode {
        LaunchMode::FitOutput => bounds
            .or(output_size.filter(|size| size.validate().is_ok()))
            .unwrap_or(PORTABLE_WINDOWED_DEFAULT),
        LaunchMode::Windowed => preference
            .size
            .unwrap_or(PORTABLE_WINDOWED_DEFAULT)
            .clamped_to(bounds),
    }
}

fn validate_schema(schema: u32) -> Result<(), WindowPolicyError> {
    if schema == SCHEMA {
        Ok(())
    } else {
        Err(WindowPolicyError::Invalid(format!(
            "unsupported schema {schema}; expected {SCHEMA}"
        )))
    }
}

fn validate_map<T>(
    applications: &BTreeMap<String, T>,
    validate_value: impl Fn(&T) -> Result<(), WindowPolicyError>,
) -> Result<(), WindowPolicyError> {
    if applications.len() > MAX_APPLICATIONS {
        return Err(WindowPolicyError::Invalid(format!(
            "document exceeds {MAX_APPLICATIONS} applications"
        )));
    }
    for (package, value) in applications {
        validate_package(package)?;
        validate_value(value)?;
    }
    Ok(())
}

fn validate_package(package: &str) -> Result<(), WindowPolicyError> {
    let valid = !package.is_empty()
        && package.len() <= 255
        && package.split('.').count() >= 2
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(WindowPolicyError::Invalid(format!(
            "invalid Android package {package:?}"
        )))
    }
}

fn absolute_path(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn read_document<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, WindowPolicyError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("inspect window policy", source)),
    };
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(WindowPolicyError::Invalid(format!(
            "{} is not a bounded regular file",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|source| io_error("read window policy", source))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_document<T: Serialize>(path: &Path, value: &T) -> Result<(), WindowPolicyError> {
    let parent = path.parent().ok_or_else(|| {
        WindowPolicyError::Invalid("window policy path has no parent directory".into())
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| io_error("create window policy directory", source))?;
    let mut encoded = serde_json::to_vec_pretty(value)?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(WindowPolicyError::Invalid(
            "serialized window policy exceeds its size limit".into(),
        ));
    }
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| io_error("create window policy", source))?;
    temporary
        .write_all(&encoded)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write window policy", source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace window policy", error.error))?;
    Ok(())
}

fn io_error(context: &'static str, source: io::Error) -> WindowPolicyError {
    WindowPolicyError::Io { context, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_default_is_moderate_and_respects_small_outputs() {
        let store = WindowPolicyStore::ephemeral();
        assert_eq!(
            store.resolve_initial(
                "com.android.settings",
                None,
                None,
                Some(LogicalSize::new(2560, 1440).unwrap()),
                None
            ),
            PORTABLE_WINDOWED_DEFAULT
        );
        assert_eq!(
            store.resolve_initial(
                "com.android.settings",
                None,
                None,
                None,
                Some(LogicalSize::new(400, 600).unwrap())
            ),
            LogicalSize::new(400, 600).unwrap()
        );
    }

    #[test]
    fn mobile_does_not_replace_desktop_size_history() {
        let mut store = WindowPolicyStore::ephemeral();
        let desktop = LogicalSize::new(620, 740).unwrap();
        store.remember("com.android.settings", desktop).unwrap();
        let mut store = store.with_session_mode(SessionMode::Mobile);
        assert!(
            !store
                .remember(
                    "com.android.settings",
                    LogicalSize::new(1200, 2400).unwrap()
                )
                .unwrap()
        );
        let store = store.with_session_mode(SessionMode::Desktop);
        assert_eq!(
            store.resolve_initial("com.android.settings", None, None, None, None),
            desktop
        );
    }

    #[test]
    fn fit_output_uses_standard_bounds_without_an_intermediate_default() {
        let store = WindowPolicyStore::ephemeral().with_session_mode(SessionMode::Mobile);
        assert_eq!(
            store.resolve_initial(
                "com.android.settings",
                None,
                None,
                Some(LogicalSize::new(1264, 2684).unwrap()),
                Some(LogicalSize::new(1264, 2780).unwrap()),
            ),
            LogicalSize::new(1264, 2684).unwrap()
        );
    }

    #[test]
    fn explicit_compositor_axes_remain_authoritative() {
        let store = WindowPolicyStore::ephemeral();
        assert_eq!(
            store.resolve_initial(
                "org.mozilla.firefox",
                Some(900),
                Some(700),
                Some(LogicalSize::new(1264, 2684).unwrap()),
                None,
            ),
            LogicalSize::new(900, 700).unwrap()
        );
    }

    #[test]
    fn stable_sizes_are_atomic_and_restore_windowed_applications() {
        let directory = tempfile::tempdir().unwrap();
        let paths = WindowPolicyPaths::new(
            directory.path().join("config/policy.json"),
            directory.path().join("state/sizes.json"),
        );
        let mut store = WindowPolicyStore::load(paths.clone()).unwrap();
        store
            .set_preference(
                None,
                WindowPreference::windowed(LogicalSize::new(640, 960).unwrap()),
            )
            .unwrap();
        let remembered = LogicalSize::new(800, 1200).unwrap();
        assert!(store.remember("com.android.settings", remembered).unwrap());
        assert!(!store.remember("com.android.settings", remembered).unwrap());

        let loaded = WindowPolicyStore::load(paths).unwrap();
        assert_eq!(
            loaded.resolve_initial("com.android.settings", None, None, None, None),
            remembered
        );
    }

    #[test]
    fn fit_output_tracks_new_bounds_instead_of_restoring_an_old_orientation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = WindowPolicyPaths::new(
            directory.path().join("config/policy.json"),
            directory.path().join("state/sizes.json"),
        );
        let mut store = WindowPolicyStore::load(paths.clone()).unwrap();
        store
            .remember("com.android.settings", LogicalSize::new(1200, 800).unwrap())
            .unwrap();

        let loaded = WindowPolicyStore::load(paths)
            .unwrap()
            .with_session_mode(SessionMode::Mobile);
        assert_eq!(
            loaded.resolve_initial(
                "com.android.settings",
                None,
                None,
                Some(LogicalSize::new(800, 1200).unwrap()),
                None,
            ),
            LogicalSize::new(800, 1200).unwrap()
        );
    }

    #[test]
    fn an_explicit_package_policy_overrides_remembered_geometry() {
        let directory = tempfile::tempdir().unwrap();
        let paths = WindowPolicyPaths::new(
            directory.path().join("config/policy.json"),
            directory.path().join("state/sizes.json"),
        );
        let mut store = WindowPolicyStore::load(paths.clone()).unwrap();
        store
            .remember("org.mozilla.firefox", LogicalSize::new(800, 1200).unwrap())
            .unwrap();
        store
            .set_preference(
                Some("org.mozilla.firefox"),
                WindowPreference::windowed(LogicalSize::new(700, 900).unwrap()),
            )
            .unwrap();

        let loaded = WindowPolicyStore::load(paths).unwrap();
        assert_eq!(
            loaded.resolve_initial("org.mozilla.firefox", None, None, None, None),
            LogicalSize::new(700, 900).unwrap()
        );
    }
}
