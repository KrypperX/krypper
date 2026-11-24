//! Per-user model defaults and provider credentials used by `krypper setup`.

use super::config::config_path;
use crate::brain::{is_provider_credential_name, validate_model_name_for_provider, ModelProvider};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CREDENTIALS_FILE: &str = "credentials.json";
const PROVIDER_PROFILES_FILE: &str = "provider-profiles.json";
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

pub(super) struct CredentialStore {
    values: BTreeMap<String, String>,
}

impl CredentialStore {
    pub(super) fn empty() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub(super) fn insert(&mut self, name: &str, value: String) {
        self.values.insert(name.to_owned(), value);
    }

    pub(super) fn remove(&mut self, name: &str) {
        self.values.remove(name);
    }

    pub(super) fn len(&self) -> usize {
        self.values.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

pub(super) struct UserSettingsPaths {
    pub(super) model: PathBuf,
    pub(super) credentials: PathBuf,
    pub(super) provider_profiles: PathBuf,
}

/// Returns the ordinary per-user paths. This deliberately ignores
/// `KRYPPER_CONFIG`, which can point inside a project and is only a read
/// override for task defaults.
pub(super) fn user_settings_paths() -> Result<Option<UserSettingsPaths>, String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let Some(model) = config_path(None, std::env::var_os("XDG_CONFIG_HOME"), home)? else {
        return Ok(None);
    };
    Ok(Some(UserSettingsPaths {
        credentials: model.with_file_name(CREDENTIALS_FILE),
        provider_profiles: model.with_file_name(PROVIDER_PROFILES_FILE),
        model,
    }))
}

pub(super) fn load_credentials(path: &Path) -> Result<Option<CredentialStore>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(file_error("cannot inspect credential store", path, cause)),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "credential store '{}' is not a regular file",
            path.display()
        ));
    }
    ensure_private_permissions(path, &metadata)?;

    let file = open_credentials(path)
        .map_err(|cause| file_error("cannot read credential store", path, cause))?;
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| file_error("cannot read credential store", path, cause))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(format!(
            "credential store '{}' is larger than {MAX_CREDENTIAL_BYTES} bytes",
            path.display()
        ));
    }
    let values: BTreeMap<String, String> = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "credential store '{}' is invalid JSON; run `krypper setup` to replace it",
            path.display()
        )
    })?;
    if values
        .iter()
        .any(|(name, value)| !is_provider_credential_name(name) || !valid_credential(value))
    {
        return Err(format!(
            "credential store '{}' contains an unsupported or invalid value",
            path.display()
        ));
    }
    Ok(Some(CredentialStore { values }))
}

pub(super) fn save_model_defaults(path: &Path, provider: &str, model: &str) -> Result<(), String> {
    let Some(provider_kind) = ModelProvider::parse(provider) else {
        return Err("unsupported provider".to_owned());
    };
    validate_model_name_for_provider(model, provider_kind)
        .map_err(|_| "invalid model ID".to_owned())?;
    let contents = format!("provider = \"{provider}\"\nmodel = \"{model}\"\n");
    write_private_file(path, contents.as_bytes(), "model configuration")
}

pub(super) fn save_credentials(path: &Path, credentials: &CredentialStore) -> Result<(), String> {
    if credentials.is_empty() {
        return remove_file_if_present(path, "credential store");
    }
    let bytes = serde_json::to_vec_pretty(&credentials.values)
        .map_err(|_| "cannot serialize credential store".to_owned())?;
    write_private_file(path, &bytes, "credential store")
}

pub(super) fn remove_settings(paths: &UserSettingsPaths) -> Result<(), String> {
    remove_file_if_present(&paths.credentials, "credential store")?;
    remove_file_if_present(&paths.provider_profiles, "provider profiles")?;
    remove_file_if_present(&paths.model, "model configuration")
}

pub(super) fn valid_credential(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8 * 1024
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

pub(super) fn write_private_file(
    path: &Path,
    bytes: &[u8],
    description: &str,
) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!("cannot locate the user configuration directory for {description}")
    })?;
    create_private_directory(parent)
        .map_err(|cause| file_error("cannot create user configuration directory", parent, cause))?;
    let metadata = fs::symlink_metadata(parent).map_err(|cause| {
        file_error("cannot inspect user configuration directory", parent, cause)
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "user configuration path '{}' is not a directory",
            parent.display()
        ));
    }
    set_private_directory_permissions(parent)?;

    if let Ok(existing) = fs::symlink_metadata(path) {
        if !existing.file_type().is_file() {
            return Err(format!(
                "cannot replace {description} '{}': it is not a regular file",
                path.display()
            ));
        }
    }

    let temp_path = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp_path)
        .map_err(|cause| file_error("cannot create temporary settings file", &temp_path, cause))?;
    if let Err(cause) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(file_error("cannot write settings file", &temp_path, cause));
    }
    if let Err(message) = set_private_file_permissions(&temp_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(message);
    }
    drop(file);
    fs::rename(&temp_path, path).map_err(|cause| {
        let _ = fs::remove_file(&temp_path);
        file_error("cannot save settings file", path, cause)
    })
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

pub(super) fn remove_file_if_present(path: &Path, description: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(file_error(
            &format!("cannot remove {description}"),
            path,
            cause,
        )),
    }
}

fn file_error(action: &str, path: &Path, cause: io::Error) -> String {
    format!("{action} '{}': {}", path.display(), cause.kind())
}

pub(super) fn ensure_private_permissions(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "credential store '{}' is accessible by other users; restrict its permissions or run `krypper setup --reset`",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            let parent_metadata = fs::symlink_metadata(parent).map_err(|cause| {
                file_error("cannot inspect credential directory", parent, cause)
            })?;
            if !parent_metadata.file_type().is_dir()
                || parent_metadata.permissions().mode() & 0o077 != 0
            {
                return Err(format!(
                    "credential directory '{}' must be private to the current user",
                    parent.display()
                ));
            }
        }
    }
    let _ = (path, metadata);
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|cause| {
            file_error("cannot restrict user configuration directory", path, cause)
        })?;
    }
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|cause| {
            file_error("cannot restrict settings file permissions", path, cause)
        })?;
    }
    let _ = path;
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_credentials(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
pub(super) fn open_credentials(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("krypper-settings-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saved_credentials_round_trip_and_error_without_secret_disclosure() {
        let directory = TempDir::new();
        let path = directory.0.join("krypper").join(CREDENTIALS_FILE);
        let secret = "test-token-never-print";
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", secret.to_owned());
        save_credentials(&path, &credentials).unwrap();

        let loaded = load_credentials(&path).unwrap().unwrap();
        assert_eq!(loaded.get("OPENAI_API_KEY"), Some(secret));
        fs::write(&path, b"not json").unwrap();
        let error = match load_credentials(&path) {
            Err(error) => error,
            Ok(_) => panic!("invalid credentials must fail to load"),
        };
        assert!(!error.contains(secret));
    }

    #[cfg(unix)]
    #[test]
    fn saved_credentials_and_parent_directory_have_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TempDir::new();
        let path = directory.0.join("krypper").join(CREDENTIALS_FILE);
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", "test-token".to_owned());
        save_credentials(&path, &credentials).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn model_defaults_are_written_without_secrets() {
        let directory = TempDir::new();
        let path = directory.0.join("krypper").join("config.toml");
        save_model_defaults(&path, "openai", "gpt-4.1").unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents, "provider = \"openai\"\nmodel = \"gpt-4.1\"\n");
        assert!(!contents.contains("API_KEY"));
    }
}
