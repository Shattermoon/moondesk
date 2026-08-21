use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const UPDATE_EXIT_CODE: i32 = 75;
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const UPDATE_STATE_SCHEMA_VERSION: u32 = 1;
const UPDATE_REQUEST_SCHEMA_VERSION: u32 = 1;
const MAX_UPDATE_STATE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateStateFile {
    schema_version: u32,
    package_name: String,
    current_version: String,
    latest_version: String,
    managed_install: bool,
    available: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequestFile<'a> {
    schema_version: u32,
    current_version: &'a str,
    target_version: &'a str,
}

fn parse_stable_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut parsed = [0_u64; 3];
    for slot in &mut parsed {
        let part = parts.next()?;
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
            return None;
        }
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(parsed)
}

fn compare_stable_versions(left: &str, right: &str) -> Option<Ordering> {
    Some(parse_stable_version(left)?.cmp(&parse_stable_version(right)?))
}

fn managed_update_paths() -> Option<(PathBuf, PathBuf)> {
    if std::env::var("MOONDESK_NPM_MANAGED").ok().as_deref() != Some("1") {
        return None;
    }
    let state = std::env::var_os("MOONDESK_UPDATE_STATE_PATH")?;
    let request = std::env::var_os("MOONDESK_UPDATE_REQUEST_PATH")?;
    Some((PathBuf::from(state), PathBuf::from(request)))
}

fn available_update_from_path(state_path: &Path, current_version: &str) -> Option<UpdateInfo> {
    let metadata = fs::metadata(state_path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_UPDATE_STATE_BYTES {
        return None;
    }
    let raw = fs::read(state_path).ok()?;
    let state: UpdateStateFile = serde_json::from_slice(&raw).ok()?;
    if state.schema_version != UPDATE_STATE_SCHEMA_VERSION
        || state.package_name != "moondesk"
        || state.current_version != current_version
        || !state.managed_install
        || !state.available
        || compare_stable_versions(&state.latest_version, current_version)
            != Some(Ordering::Greater)
    {
        return None;
    }
    Some(UpdateInfo {
        current_version: state.current_version,
        latest_version: state.latest_version,
    })
}

pub fn available_update() -> Option<UpdateInfo> {
    let (state_path, _) = managed_update_paths()?;
    available_update_from_path(&state_path, CURRENT_VERSION)
}

fn write_update_request_to_path(
    request_path: &Path,
    current_version: &str,
    target_version: &str,
) -> std::io::Result<()> {
    if compare_stable_versions(target_version, current_version) != Some(Ordering::Greater) {
        return Err(std::io::Error::other(format!(
            "invalid MoonDesk update target {target_version}"
        )));
    }
    if let Some(parent) = request_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let request = UpdateRequestFile {
        schema_version: UPDATE_REQUEST_SCHEMA_VERSION,
        current_version,
        target_version,
    };
    let bytes = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
    write_private_create_new(request_path, &bytes)
}

fn write_validated_update_request(
    state_path: &Path,
    request_path: &Path,
    current_version: &str,
    target_version: &str,
) -> std::io::Result<()> {
    let update = available_update_from_path(state_path, current_version).ok_or_else(|| {
        std::io::Error::other("MoonDesk does not have a validated global npm update available")
    })?;
    if update.latest_version != target_version {
        return Err(std::io::Error::other(format!(
            "MoonDesk update target changed from {} to {target_version}",
            update.latest_version
        )));
    }
    write_update_request_to_path(request_path, current_version, target_version)
}

pub fn write_update_request(target_version: &str) -> std::io::Result<()> {
    let (state_path, request_path) = managed_update_paths().ok_or_else(|| {
        std::io::Error::other(
            "MoonDesk self-update is only available from the npm-managed launcher",
        )
    })?;
    write_validated_update_request(&state_path, &request_path, CURRENT_VERSION, target_version)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "could not write MoonDesk update request {}: {error}",
                    request_path.display()
                ),
            )
        })
}

fn write_private_create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        UpdateInfo, available_update_from_path, compare_stable_versions, parse_stable_version,
        write_update_request_to_path, write_validated_update_request,
    };
    use std::cmp::Ordering;
    use std::fs;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create update test directory");
        dir
    }

    #[test]
    fn stable_version_parser_is_strict() {
        assert_eq!(parse_stable_version("1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_stable_version("0.0.0"), Some([0, 0, 0]));
        assert_eq!(parse_stable_version("01.2.3"), None);
        assert_eq!(parse_stable_version("1.2"), None);
        assert_eq!(parse_stable_version("1.2.3-beta.1"), None);
        assert_eq!(parse_stable_version("1.2.3.4"), None);
    }

    #[test]
    fn stable_version_comparison_handles_release_order() {
        assert_eq!(
            compare_stable_versions("1.2.3", "1.2.4"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_stable_versions("2.0.0", "1.99.99"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_stable_versions("3.4.5", "3.4.5"),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn update_state_requires_matching_package_current_version_and_newer_target() {
        let dir = temp_dir("moondesk-update-state");
        let state_path = dir.join("state.json");
        let valid = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.3",
            "latestVersion": "1.2.4",
            "managedInstall": true,
            "available": true,
            "checkedAt": "2026-08-21T12:00:00.000Z"
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&valid).expect("encode state"),
        )
        .expect("write state");
        assert_eq!(
            available_update_from_path(&state_path, "1.2.3"),
            Some(UpdateInfo {
                current_version: "1.2.3".into(),
                latest_version: "1.2.4".into()
            })
        );

        let unmanaged = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.3",
            "latestVersion": "1.2.4",
            "managedInstall": false,
            "available": true
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&unmanaged).expect("encode unmanaged state"),
        )
        .expect("write unmanaged state");
        assert_eq!(available_update_from_path(&state_path, "1.2.3"), None);

        let wrong_current = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.2",
            "latestVersion": "1.2.4",
            "managedInstall": true,
            "available": true
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&wrong_current).expect("encode wrong state"),
        )
        .expect("write wrong state");
        assert_eq!(available_update_from_path(&state_path, "1.2.3"), None);

        let not_newer = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.3",
            "latestVersion": "1.2.3",
            "managedInstall": true,
            "available": true
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&not_newer).expect("encode same state"),
        )
        .expect("write same state");
        assert_eq!(available_update_from_path(&state_path, "1.2.3"), None);

        let rejected = [
            serde_json::json!({
                "schemaVersion": 1,
                "packageName": "not-moondesk",
                "currentVersion": "1.2.3",
                "latestVersion": "1.2.4",
                "managedInstall": true,
                "available": true
            }),
            serde_json::json!({
                "schemaVersion": 99,
                "packageName": "moondesk",
                "currentVersion": "1.2.3",
                "latestVersion": "1.2.4",
                "managedInstall": true,
                "available": true
            }),
            serde_json::json!({
                "schemaVersion": 1,
                "packageName": "moondesk",
                "currentVersion": "1.2.3",
                "latestVersion": "1.2.4",
                "managedInstall": true,
                "available": false
            }),
        ];
        for state in rejected {
            fs::write(
                &state_path,
                serde_json::to_vec(&state).expect("encode rejected state"),
            )
            .expect("write rejected state");
            assert_eq!(
                available_update_from_path(&state_path, "1.2.3"),
                None,
                "state must be rejected: {state}"
            );
        }

        let oversized = dir.join("oversized.json");
        fs::write(&oversized, vec![b' '; 17 * 1024]).expect("write oversized update state");
        assert_eq!(available_update_from_path(&oversized, "1.2.3"), None);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validated_update_request_requires_the_exact_managed_available_target() {
        let dir = temp_dir("moondesk-validated-update-request");
        let state_path = dir.join("state.json");
        let request_path = dir.join("request.json");
        let state = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.3",
            "latestVersion": "1.2.4",
            "managedInstall": true,
            "available": true
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&state).expect("encode validated state"),
        )
        .expect("write validated state");

        assert!(
            write_validated_update_request(
                &state_path,
                &dir.join("wrong-target.json"),
                "1.2.3",
                "1.2.5"
            )
            .is_err()
        );
        write_validated_update_request(&state_path, &request_path, "1.2.3", "1.2.4")
            .expect("write exact validated update request");

        let unmanaged = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "currentVersion": "1.2.3",
            "latestVersion": "1.2.4",
            "managedInstall": false,
            "available": true
        });
        fs::write(
            &state_path,
            serde_json::to_vec(&unmanaged).expect("encode unmanaged state"),
        )
        .expect("write unmanaged state");
        assert!(
            write_validated_update_request(
                &state_path,
                &dir.join("unmanaged.json"),
                "1.2.3",
                "1.2.4"
            )
            .is_err()
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_request_is_create_new_exact_and_rejects_non_upgrades() {
        let dir = temp_dir("moondesk-update-request");
        let request_path = dir.join("requests").join("request.json");
        write_update_request_to_path(&request_path, "1.2.3", "1.2.4")
            .expect("write valid update request");
        let request: serde_json::Value =
            serde_json::from_slice(&fs::read(&request_path).expect("read update request"))
                .expect("decode update request");
        assert_eq!(request["schemaVersion"], 1);
        assert_eq!(request["currentVersion"], "1.2.3");
        assert_eq!(request["targetVersion"], "1.2.4");

        assert!(write_update_request_to_path(&request_path, "1.2.3", "1.2.5").is_err());
        assert!(write_update_request_to_path(&dir.join("same.json"), "1.2.3", "1.2.3").is_err());
        assert!(
            write_update_request_to_path(&dir.join("pre.json"), "1.2.3", "1.2.4-beta.1").is_err()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&request_path)
                .expect("request metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(dir);
    }
}
