//! The saved login.
//!
//! It is the same file the ContextStream MCP keeps (`~/.contextstream/credentials.json`), so
//! someone signed in there is signed in here, and the other way round. The file holds an API
//! key and, for a non-default API, its address. It is written with owner-only permissions,
//! through a temporary file, and never through a symlink. Keys this file holds that this
//! program does not know are kept as they are.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::DEFAULT_API_URL;

/// What the saved login holds.
#[derive(Clone, Default)]
pub struct Saved {
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub email: Option<String>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("api_url", &self.api_url)
            .field("email", &self.email)
            .finish()
    }
}

/// `~/.contextstream/credentials.json`.
pub fn path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream").join("credentials.json"))
}

/// The saved login, when there is a readable one.
pub fn read() -> Option<Saved> {
    read_from(&path()?)
}

pub fn read_from(path: &Path) -> Option<Saved> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 1 << 20 {
        return None;
    }
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Some(Saved {
        api_key: text("api_key"),
        api_url: text("api_url"),
        email: text("email"),
    })
}

/// Save `api_key` (and `api_url` when it is not the default API).
pub fn write(api_key: &str, api_url: &str, email: Option<&str>) -> std::io::Result<PathBuf> {
    let path = path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home folder"))?;
    write_to(&path, api_key, api_url, email)?;
    Ok(path)
}

pub fn write_to(
    path: &Path,
    api_key: &str,
    api_url: &str,
    email: Option<&str>,
) -> std::io::Result<()> {
    let invalid = |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let mut object = match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid("the credentials path is not a regular file"))
        }
        Ok(_) => match serde_json::from_str::<Value>(&std::fs::read_to_string(path)?) {
            Ok(Value::Object(object)) => object,
            // A file that is not a JSON object is not one to merge into.
            _ => return Err(invalid("the credentials file is not a JSON object")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(error),
    };
    object.insert("api_key".into(), json!(api_key));
    object.remove("jwt");
    let api_url = api_url.trim().trim_end_matches('/');
    if api_url.is_empty() || api_url == DEFAULT_API_URL {
        object.remove("api_url");
    } else {
        object.insert("api_url".into(), json!(api_url));
    }
    match email {
        Some(email) => {
            object.insert("email".into(), json!(email));
        }
        None => {
            object.remove("email");
        }
    }
    object.insert("saved_at".into(), json!(chrono::Utc::now().to_rfc3339()));

    let parent = path
        .parent()
        .ok_or_else(|| invalid("the credentials path has no folder"))?;
    create_private_dir(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    set_private(temporary.path())?;
    temporary.write_all(serde_json::to_string_pretty(&Value::Object(object))?.as_bytes())?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    set_private(path)
}

/// Forget the saved login. Returns whether there was one.
pub fn delete() -> std::io::Result<bool> {
    match path() {
        Some(path) => delete_at(&path),
        None => Ok(false),
    }
}

pub fn delete_at(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Owner read and write only.
pub fn set_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_login_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("credentials.json");
        write_to(
            &path,
            "example-key-1",
            DEFAULT_API_URL,
            Some("dev@example.test"),
        )
        .unwrap();
        let saved = read_from(&path).unwrap();
        assert_eq!(saved.api_key.as_deref(), Some("example-key-1"));
        assert_eq!(saved.api_url, None, "the default API address is not stored");
        assert_eq!(saved.email.as_deref(), Some("dev@example.test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_custom_api_address_is_kept_and_unknown_keys_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, r#"{"api_key":"old","jwt":"stale","note":"keep me"}"#).unwrap();
        write_to(&path, "example-key-2", "http://127.0.0.1:9000/", None).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["api_key"], "example-key-2");
        assert_eq!(raw["api_url"], "http://127.0.0.1:9000");
        assert_eq!(raw["note"], "keep me");
        assert!(raw.get("jwt").is_none(), "a stale token is dropped");
        assert!(raw.get("email").is_none());
    }

    #[test]
    fn deleting_reports_whether_there_was_a_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        assert!(!delete_at(&path).unwrap());
        write_to(&path, "example-key-3", DEFAULT_API_URL, None).unwrap();
        assert!(delete_at(&path).unwrap());
        assert!(read_from(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, "{}").unwrap();
        let path = dir.path().join("credentials.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(write_to(&path, "example-key-4", DEFAULT_API_URL, None).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
        assert!(read_from(&path).is_none());
    }

    #[test]
    fn a_file_that_is_not_an_object_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(write_to(&path, "example-key-5", DEFAULT_API_URL, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1, 2]");
    }

    #[test]
    fn debug_output_hides_the_key() {
        let saved = Saved {
            api_key: Some("example-key-6".into()),
            ..Default::default()
        };
        assert!(!format!("{saved:?}").contains("example-key-6"));
    }
}
