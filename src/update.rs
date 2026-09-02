use crate::state::user_home_dir;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const UPDATE_EXIT_CODE: i32 = 75;
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const UPDATE_STATE_SCHEMA_VERSION: u32 = 1;
const UPDATE_REQUEST_SCHEMA_VERSION: u32 = 1;
const CHANGELOG_NOTICE_SCHEMA_VERSION: u32 = 1;
const MAX_UPDATE_STATE_BYTES: u64 = 16 * 1024;
const MAX_CHANGELOG_NOTICE_BYTES: u64 = 16 * 1024;
const MAX_RELEASE_NOTES: usize = 12;
const MAX_RELEASE_NOTE_CHARS: usize = 180;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_notes: Vec<String>,
    pub release_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangelogNotice {
    pub from_version: String,
    pub to_version: String,
    pub release_notes: Vec<String>,
    pub release_url: Option<String>,
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
    #[serde(default)]
    release_notes: Vec<String>,
    #[serde(default)]
    release_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangelogNoticeFile {
    schema_version: u32,
    package_name: String,
    from_version: String,
    to_version: String,
    #[serde(default)]
    release_notes: Vec<String>,
    #[serde(default)]
    release_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequestFile<'a> {
    schema_version: u32,
    current_version: &'a str,
    target_version: &'a str,
    release_notes: &'a [String],
    release_url: Option<&'a str>,
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

fn validated_release_notes(notes: Vec<String>) -> Vec<String> {
    notes
        .into_iter()
        .filter_map(|note| {
            let trimmed = note.trim();
            if trimmed.is_empty()
                || trimmed.chars().count() > MAX_RELEASE_NOTE_CHARS
                || trimmed.chars().any(char::is_control)
            {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .take(MAX_RELEASE_NOTES)
        .collect()
}

fn validated_release_url(value: Option<String>, version: &str) -> Option<String> {
    let expected = format!("https://github.com/Shattermoon/moondesk/releases/tag/v{version}");
    value.filter(|url| url == &expected)
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
    let latest_version = state.latest_version;
    Some(UpdateInfo {
        current_version: state.current_version,
        release_notes: validated_release_notes(state.release_notes),
        release_url: validated_release_url(state.release_url, &latest_version),
        latest_version,
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
    release_notes: &[String],
    release_url: Option<&str>,
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
        release_notes,
        release_url,
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
    write_update_request_to_path(
        request_path,
        current_version,
        target_version,
        &update.release_notes,
        update.release_url.as_deref(),
    )
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

fn pending_changelog_notice_from_path(
    notice_path: &Path,
    current_version: &str,
) -> Option<ChangelogNotice> {
    let metadata = fs::metadata(notice_path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CHANGELOG_NOTICE_BYTES {
        return None;
    }
    let raw = fs::read(notice_path).ok()?;
    let notice: ChangelogNoticeFile = serde_json::from_slice(&raw).ok()?;
    if notice.schema_version != CHANGELOG_NOTICE_SCHEMA_VERSION
        || notice.package_name != "moondesk"
        || notice.to_version != current_version
        || compare_stable_versions(&notice.to_version, &notice.from_version)
            != Some(Ordering::Greater)
    {
        return None;
    }
    Some(ChangelogNotice {
        from_version: notice.from_version,
        release_notes: validated_release_notes(notice.release_notes),
        release_url: validated_release_url(notice.release_url, &notice.to_version),
        to_version: notice.to_version,
    })
}

fn changelog_notice_path(current_version: &str) -> std::io::Result<PathBuf> {
    if parse_stable_version(current_version).is_none() {
        return Err(std::io::Error::other(format!(
            "invalid MoonDesk changelog version {current_version}"
        )));
    }
    Ok(user_home_dir()?
        .join(".moondesk")
        .join("updates")
        .join(format!("v{current_version}"))
        .join("post-update.json"))
}

fn changelog_notice_path_with_override(
    current_version: &str,
    override_path: Option<PathBuf>,
) -> std::io::Result<PathBuf> {
    if let Some(path) = override_path.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(path);
    }
    changelog_notice_path(current_version)
}

fn active_changelog_notice_path() -> std::io::Result<PathBuf> {
    let override_path = std::env::var_os("MOONDESK_CHANGELOG_NOTICE_PATH").map(PathBuf::from);
    changelog_notice_path_with_override(CURRENT_VERSION, override_path)
}

pub fn pending_changelog_notice() -> Option<ChangelogNotice> {
    let path = active_changelog_notice_path().ok()?;
    pending_changelog_notice_from_path(&path, CURRENT_VERSION)
}

pub fn dismiss_pending_changelog_notice() -> std::io::Result<()> {
    let path = active_changelog_notice_path()?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
        ChangelogNotice, UpdateInfo, available_update_from_path, changelog_notice_path,
        changelog_notice_path_with_override, compare_stable_versions, parse_stable_version,
        pending_changelog_notice_from_path, write_update_request_to_path,
        write_validated_update_request,
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
    fn changelog_notice_path_is_fixed_under_the_current_moondesk_update_tree() {
        let path = changelog_notice_path("1.2.4").expect("resolve changelog notice path");
        assert!(
            path.ends_with(
                std::path::Path::new(".moondesk")
                    .join("updates")
                    .join("v1.2.4")
                    .join("post-update.json")
            )
        );
        assert!(changelog_notice_path("1.2.4-beta.1").is_err());
    }

    #[test]
    fn changelog_notice_path_override_wins_over_home_derived_path() {
        let override_path = std::path::PathBuf::from("C:/custom/moondesk/post-update.json");
        assert_eq!(
            changelog_notice_path_with_override("1.2.4", Some(override_path.clone()))
                .expect("resolve explicit changelog notice path"),
            override_path
        );
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
            "releaseNotes": ["First change", "Second change"],
            "releaseUrl": "https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4",
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
                latest_version: "1.2.4".into(),
                release_notes: vec!["First change".into(), "Second change".into()],
                release_url: Some(
                    "https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into(),
                ),
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
        let notes = vec!["Polished changelog".to_string()];
        let release_url = "https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4";
        write_update_request_to_path(&request_path, "1.2.3", "1.2.4", &notes, Some(release_url))
            .expect("write valid update request");
        let request: serde_json::Value =
            serde_json::from_slice(&fs::read(&request_path).expect("read update request"))
                .expect("decode update request");
        assert_eq!(request["schemaVersion"], 1);
        assert_eq!(request["currentVersion"], "1.2.3");
        assert_eq!(request["targetVersion"], "1.2.4");
        assert_eq!(request["releaseNotes"][0], "Polished changelog");
        assert_eq!(request["releaseUrl"], release_url);

        assert!(write_update_request_to_path(&request_path, "1.2.3", "1.2.5", &[], None).is_err());
        assert!(
            write_update_request_to_path(&dir.join("same.json"), "1.2.3", "1.2.3", &[], None)
                .is_err()
        );
        assert!(
            write_update_request_to_path(
                &dir.join("pre.json"),
                "1.2.3",
                "1.2.4-beta.1",
                &[],
                None,
            )
            .is_err()
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
    #[test]
    fn post_update_notice_is_version_bound_and_bounded() {
        let dir = temp_dir("moondesk-changelog-notice");
        let notice_path = dir.join("post-update.json");
        let notice = serde_json::json!({
            "schemaVersion": 1,
            "packageName": "moondesk",
            "fromVersion": "1.2.3",
            "toVersion": "1.2.4",
            "releaseNotes": ["First change", "Second change"],
            "releaseUrl": "https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4"
        });
        fs::write(
            &notice_path,
            serde_json::to_vec(&notice).expect("encode notice"),
        )
        .expect("write notice");
        assert_eq!(
            pending_changelog_notice_from_path(&notice_path, "1.2.4"),
            Some(ChangelogNotice {
                from_version: "1.2.3".into(),
                to_version: "1.2.4".into(),
                release_notes: vec!["First change".into(), "Second change".into()],
                release_url: Some(
                    "https://github.com/Shattermoon/moondesk/releases/tag/v1.2.4".into(),
                ),
            })
        );
        assert_eq!(
            pending_changelog_notice_from_path(&notice_path, "1.2.5"),
            None
        );
        let _ = fs::remove_dir_all(dir);
    }
}
