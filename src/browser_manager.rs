use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const MANIFEST_JSON: &str = include_str!("../browser/managed-browser.json");
const MAX_BROWSER_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub struct ManagedBrowser {
    pub executable: PathBuf,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserManifest {
    schema_version: u64,
    version: String,
    revision: String,
    platforms: HashMap<String, BrowserArtifact>,
}

#[derive(Clone, Debug, Deserialize)]
struct BrowserArtifact {
    url: String,
    sha256: String,
    size: u64,
    executable: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserInstallMarker {
    schema_version: u64,
    version: String,
    platform: String,
    archive_sha256: String,
    executable: String,
    executable_sha256: String,
    executable_size: u64,
}

pub fn platform_key() -> Result<&'static str, String> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Ok("win32-x64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok("linux-x64");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Ok("linux-arm64");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok("darwin-x64");
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok("darwin-arm64");
    }
    #[allow(unreachable_code)]
    Err(format!(
        "MoonDesk's managed browser is not published for {}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

fn manifest() -> Result<BrowserManifest, String> {
    let manifest: BrowserManifest = serde_json::from_str(MANIFEST_JSON)
        .map_err(|error| format!("MoonDesk's managed-browser manifest is invalid: {error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "Unsupported MoonDesk managed-browser manifest schema {}",
            manifest.schema_version
        ));
    }
    if manifest.version.trim().is_empty() || manifest.revision.trim().is_empty() {
        return Err(
            "MoonDesk's managed-browser manifest is missing its pinned version".to_string(),
        );
    }
    for (platform, artifact) in &manifest.platforms {
        validate_artifact(platform, artifact, &manifest.version)?;
    }
    Ok(manifest)
}

fn validate_artifact(
    platform: &str,
    artifact: &BrowserArtifact,
    version: &str,
) -> Result<(), String> {
    let url = reqwest::Url::parse(&artifact.url)
        .map_err(|error| format!("Invalid managed-browser URL for {platform}: {error}"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("storage.googleapis.com")
        || !url
            .path()
            .starts_with(&format!("/chrome-for-testing-public/{version}/"))
    {
        return Err(format!(
            "Managed-browser URL for {platform} must use the pinned Chrome for Testing HTTPS origin"
        ));
    }
    if artifact.size == 0 || artifact.size > MAX_BROWSER_ARCHIVE_BYTES {
        return Err(format!(
            "Managed-browser archive size for {platform} is outside MoonDesk's safety bound"
        ));
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "Managed-browser SHA-256 for {platform} is not a 64-character hex digest"
        ));
    }
    let executable = Path::new(&artifact.executable);
    if executable.is_absolute()
        || executable.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "Managed-browser executable path for {platform} is not safely relative"
        ));
    }
    Ok(())
}

fn browser_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MOONDESK_BROWSER_CACHE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return Ok(path);
    }
    crate::state::moondesk_data_dir()
        .map(|path| path.join("browser"))
        .map_err(|error| format!("Could not resolve MoonDesk browser cache: {error}"))
}

fn install_dir_at(root: &Path, manifest: &BrowserManifest, platform: &str) -> PathBuf {
    root.join(&manifest.version).join(platform)
}

fn installed_from(
    manifest: &BrowserManifest,
    platform: &str,
    artifact: &BrowserArtifact,
) -> Result<Option<ManagedBrowser>, String> {
    let browser_root = browser_root()?;
    installed_from_root(&browser_root, manifest, platform, artifact)
}

fn installed_from_root(
    browser_root: &Path,
    manifest: &BrowserManifest,
    platform: &str,
    artifact: &BrowserArtifact,
) -> Result<Option<ManagedBrowser>, String> {
    let root = install_dir_at(browser_root, manifest, platform);
    let executable = root.join(&artifact.executable);
    if !executable.is_file() {
        return Ok(None);
    }

    let marker_path = root.join(".moondesk-browser-install.json");
    let marker: BrowserInstallMarker = match std::fs::read_to_string(&marker_path) {
        Ok(value) => match serde_json::from_str(&value) {
            Ok(marker) => marker,
            Err(_) => return Ok(None),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Could not read managed-browser verification marker {}: {error}",
                marker_path.display()
            ));
        }
    };
    if marker.schema_version != 1
        || marker.version != manifest.version
        || marker.platform != platform
        || !marker.archive_sha256.eq_ignore_ascii_case(&artifact.sha256)
        || marker.executable != artifact.executable
    {
        return Ok(None);
    }

    let metadata = std::fs::metadata(&executable).map_err(|error| {
        format!(
            "Could not inspect managed-browser executable {}: {error}",
            executable.display()
        )
    })?;
    if metadata.len() != marker.executable_size {
        return Ok(None);
    }
    let executable_sha256 = sha256_file(&executable)?;
    if !executable_sha256.eq_ignore_ascii_case(&marker.executable_sha256) {
        return Ok(None);
    }

    Ok(Some(ManagedBrowser {
        executable,
        version: manifest.version.clone(),
    }))
}

pub fn installed_browser() -> Result<Option<ManagedBrowser>, String> {
    let manifest = manifest()?;
    let platform = platform_key()?;
    let artifact = manifest.platforms.get(platform).ok_or_else(|| {
        format!("MoonDesk's managed-browser manifest has no artifact for {platform}")
    })?;
    installed_from(&manifest, platform, artifact)
}

pub async fn ensure_browser() -> Result<ManagedBrowser, String> {
    let manifest = manifest()?;
    let platform = platform_key()?;
    let artifact = manifest.platforms.get(platform).cloned().ok_or_else(|| {
        format!("MoonDesk's managed-browser manifest has no artifact for {platform}")
    })?;

    if let Some(browser) = installed_from(&manifest, platform, &artifact)? {
        return Ok(browser);
    }

    let root = browser_root()?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("Could not create MoonDesk browser cache: {error}"))?;
    set_private_dir_permissions(&root);

    let lock_path = root.join(".install.lock");
    let lock = tokio::task::spawn_blocking(move || acquire_install_lock(&lock_path))
        .await
        .map_err(|error| format!("Managed-browser lock task failed: {error}"))??;

    if let Some(browser) = installed_from(&manifest, platform, &artifact)? {
        drop(lock);
        return Ok(browser);
    }

    let version_root = root.join(&manifest.version);
    std::fs::create_dir_all(&version_root)
        .map_err(|error| format!("Could not create managed-browser version cache: {error}"))?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let archive_path = version_root.join(format!(".{platform}-{nonce}.zip"));
    let staging_dir = version_root.join(format!(".{platform}-{nonce}.staging"));
    let backup_dir = version_root.join(format!(".{platform}-{nonce}.backup"));
    let final_dir = version_root.join(platform);

    let install_result = async {
        download_archive(&artifact, &archive_path).await?;
        let archive_for_extract = archive_path.clone();
        let staging_for_extract = staging_dir.clone();
        tokio::task::spawn_blocking(move || {
            extract_archive(&archive_for_extract, &staging_for_extract)
        })
        .await
        .map_err(|error| format!("Managed-browser extraction task failed: {error}"))??;

        let executable = staging_dir.join(&artifact.executable);
        if !executable.is_file() {
            return Err(format!(
                "Managed-browser archive did not contain expected executable {}",
                artifact.executable
            ));
        }
        ensure_executable_permissions(&executable)?;
        let executable_metadata = std::fs::metadata(&executable).map_err(|error| {
            format!(
                "Could not inspect staged managed-browser executable {}: {error}",
                executable.display()
            )
        })?;
        let executable_sha256 = sha256_file(&executable)?;
        let marker = BrowserInstallMarker {
            schema_version: 1,
            version: manifest.version.clone(),
            platform: platform.to_string(),
            archive_sha256: artifact.sha256.clone(),
            executable: artifact.executable.clone(),
            executable_sha256,
            executable_size: executable_metadata.len(),
        };
        let marker_json = serde_json::to_string_pretty(&marker).map_err(|error| {
            format!("Could not encode managed-browser verification marker: {error}")
        })?;

        let had_previous = final_dir.exists();
        if had_previous {
            std::fs::rename(&final_dir, &backup_dir).map_err(|error| {
                format!(
                    "Could not stage previous managed-browser install {} for replacement: {error}",
                    final_dir.display()
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&staging_dir, &final_dir) {
            if had_previous {
                let _ = std::fs::rename(&backup_dir, &final_dir);
            }
            return Err(format!(
                "Could not atomically publish managed browser {}: {error}",
                final_dir.display()
            ));
        }

        let marker_path = final_dir.join(".moondesk-browser-install.json");
        if let Err(error) = write_atomic_text(&marker_path, &format!("{marker_json}\n")) {
            let _ = std::fs::remove_dir_all(&final_dir);
            if had_previous {
                let _ = std::fs::rename(&backup_dir, &final_dir);
            }
            return Err(error);
        }

        let published_executable = final_dir.join(&artifact.executable);
        if !published_executable.is_file() {
            let _ = std::fs::remove_dir_all(&final_dir);
            if had_previous {
                let _ = std::fs::rename(&backup_dir, &final_dir);
            }
            return Err(format!(
                "Managed browser disappeared during installation: {}",
                published_executable.display()
            ));
        }
        if had_previous {
            let _ = std::fs::remove_dir_all(&backup_dir);
        }
        Ok(ManagedBrowser {
            executable: published_executable,
            version: manifest.version.clone(),
        })
    }
    .await;

    let _ = std::fs::remove_file(&archive_path);
    if install_result.is_err() {
        let _ = std::fs::remove_dir_all(&staging_dir);
    }
    drop(lock);
    install_result
}

fn acquire_install_lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            format!(
                "Could not open MoonDesk browser installation lock {}: {error}",
                path.display()
            )
        })?;
    file.lock_exclusive().map_err(|error| {
        format!(
            "Could not lock MoonDesk browser installation {}: {error}",
            path.display()
        )
    })?;
    Ok(file)
}

async fn download_archive(artifact: &BrowserArtifact, destination: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(format!("MoonDesk/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("Could not create managed-browser download client: {error}"))?;
    let mut response = client
        .get(&artifact.url)
        .send()
        .await
        .map_err(|error| format!("Could not download MoonDesk's managed browser: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Managed-browser download returned HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size != artifact.size || size > MAX_BROWSER_ARCHIVE_BYTES)
    {
        return Err("Managed-browser server returned an unexpected archive size".to_string());
    }

    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| format!("Could not create managed-browser download: {error}"))?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Managed-browser download failed: {error}"))?
    {
        total = total.saturating_add(chunk.len() as u64);
        if total > artifact.size || total > MAX_BROWSER_ARCHIVE_BYTES {
            return Err("Managed-browser download exceeded its pinned size".to_string());
        }
        digest.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("Could not write managed-browser download: {error}"))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("Could not flush managed-browser download: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync managed-browser download: {error}"))?;
    if total != artifact.size {
        return Err(format!(
            "Managed-browser download size mismatch: expected {}, received {total}",
            artifact.size
        ));
    }
    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(format!(
            "Managed-browser SHA-256 mismatch: expected {}, received {actual}",
            artifact.sha256
        ));
    }
    Ok(())
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        std::fs::remove_dir_all(destination).map_err(|error| {
            format!(
                "Could not clean managed-browser staging directory {}: {error}",
                destination.display()
            )
        })?;
    }
    std::fs::create_dir_all(destination).map_err(|error| {
        format!(
            "Could not create managed-browser staging directory {}: {error}",
            destination.display()
        )
    })?;

    let archive_file = File::open(archive_path).map_err(|error| {
        format!(
            "Could not open managed-browser archive {}: {error}",
            archive_path.display()
        )
    })?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .map_err(|error| format!("Managed-browser ZIP is invalid: {error}"))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("Could not read managed-browser ZIP entry: {error}"))?;
        let relative = entry.enclosed_name().ok_or_else(|| {
            format!(
                "Managed-browser ZIP entry has an unsafe path: {}",
                entry.name()
            )
        })?;
        let output_path = destination.join(relative);
        if !output_path.starts_with(destination) {
            return Err(format!(
                "Managed-browser ZIP entry escaped staging directory: {}",
                entry.name()
            ));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&output_path).map_err(|error| {
                format!(
                    "Could not create managed-browser directory {}: {error}",
                    output_path.display()
                )
            })?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "Could not create managed-browser directory {}: {error}",
                    parent.display()
                )
            })?;
        }

        if entry.is_symlink() {
            extract_safe_symlink(&mut entry, destination, &output_path)?;
            continue;
        }

        let mut output = File::create(&output_path).map_err(|error| {
            format!(
                "Could not create managed-browser file {}: {error}",
                output_path.display()
            )
        })?;
        std::io::copy(&mut entry, &mut output).map_err(|error| {
            format!(
                "Could not extract managed-browser file {}: {error}",
                output_path.display()
            )
        })?;
        output.sync_all().map_err(|error| {
            format!(
                "Could not sync managed-browser file {}: {error}",
                output_path.display()
            )
        })?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| {
                format!(
                    "Could not restore managed-browser permissions {}: {error}",
                    output_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn extract_safe_symlink<R: Read>(
    entry: &mut zip::read::ZipFile<'_, R>,
    root: &Path,
    output_path: &Path,
) -> Result<(), String> {
    let mut target = String::new();
    entry
        .read_to_string(&mut target)
        .map_err(|error| format!("Could not read managed-browser symlink target: {error}"))?;
    let target = Path::new(target.trim());
    if target.is_absolute() {
        return Err(format!(
            "Managed-browser archive contains absolute symlink {} -> {}",
            output_path.display(),
            target.display()
        ));
    }
    let parent = output_path.parent().ok_or_else(|| {
        format!(
            "Managed-browser symlink has no parent directory: {}",
            output_path.display()
        )
    })?;
    if !lexical_path_within(root, parent, target) {
        return Err(format!(
            "Managed-browser archive symlink escapes installation root: {} -> {}",
            output_path.display(),
            target.display()
        ));
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, output_path).map_err(|error| {
            format!(
                "Could not create managed-browser symlink {}: {error}",
                output_path.display()
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (root, output_path, target);
        Err("Managed-browser archive unexpectedly contains a symlink on this platform".to_string())
    }
}

fn lexical_path_within(root: &Path, base: &Path, target: &Path) -> bool {
    let Ok(base_relative) = base.strip_prefix(root) else {
        return false;
    };
    let mut depth = base_relative
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Component::Prefix(_) | Component::RootDir => return false,
        }
    }
    true
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| {
        format!(
            "Could not open managed-browser file {} for verification: {error}",
            path.display()
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            format!(
                "Could not read managed-browser file {} for verification: {error}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn write_atomic_text(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Managed-browser verification marker has no parent: {}",
            path.display()
        )
    })?;
    let temporary = parent.join(format!(".marker-{}.tmp", uuid::Uuid::new_v4().simple()));
    {
        let mut file = File::create(&temporary).map_err(|error| {
            format!(
                "Could not create managed-browser verification marker {}: {error}",
                temporary.display()
            )
        })?;
        file.write_all(contents.as_bytes()).map_err(|error| {
            format!(
                "Could not write managed-browser verification marker {}: {error}",
                temporary.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "Could not sync managed-browser verification marker {}: {error}",
                temporary.display()
            )
        })?;
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!(
            "Could not publish managed-browser verification marker {}: {error}",
            path.display()
        )
    })
}

fn ensure_executable_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).map_err(|error| {
            format!(
                "Could not inspect managed-browser executable {}: {error}",
                path.display()
            )
        })?;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o755)).map_err(
                |error| {
                    format!(
                        "Could not mark managed-browser executable {} executable: {error}",
                        path.display()
                    )
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_private_dir_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_browser_manifest_is_pinned_and_platform_complete() {
        let manifest = manifest().unwrap_or_else(|error| panic!("manifest: {error}"));
        assert_eq!(manifest.schema_version, 1);
        assert!(!manifest.version.is_empty());
        for platform in [
            "linux-x64",
            "linux-arm64",
            "darwin-x64",
            "darwin-arm64",
            "win32-x64",
        ] {
            assert!(
                manifest.platforms.contains_key(platform),
                "missing managed browser platform {platform}"
            );
        }
    }

    #[test]
    fn cached_browser_rejects_executable_tampering() {
        let root = std::env::temp_dir().join(format!(
            "moondesk-browser-integrity-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let manifest = BrowserManifest {
            schema_version: 1,
            version: "test-browser".to_string(),
            revision: "test-revision".to_string(),
            platforms: HashMap::new(),
        };
        let artifact = BrowserArtifact {
            url: "https://storage.googleapis.com/chrome-for-testing-public/test-browser/test.zip"
                .to_string(),
            sha256: "a".repeat(64),
            size: 1,
            executable: "chrome/chrome.exe".to_string(),
        };
        let install_root = install_dir_at(&root, &manifest, "test-platform");
        let executable = install_root.join(&artifact.executable);
        std::fs::create_dir_all(
            executable
                .parent()
                .unwrap_or_else(|| panic!("test executable parent")),
        )
        .unwrap_or_else(|error| panic!("create test browser cache: {error}"));
        std::fs::write(&executable, b"verified-browser")
            .unwrap_or_else(|error| panic!("write test browser: {error}"));
        let metadata = std::fs::metadata(&executable)
            .unwrap_or_else(|error| panic!("test browser metadata: {error}"));
        let marker = BrowserInstallMarker {
            schema_version: 1,
            version: manifest.version.clone(),
            platform: "test-platform".to_string(),
            archive_sha256: artifact.sha256.clone(),
            executable: artifact.executable.clone(),
            executable_sha256: sha256_file(&executable)
                .unwrap_or_else(|error| panic!("hash test browser: {error}")),
            executable_size: metadata.len(),
        };
        let marker_json = serde_json::to_string(&marker)
            .unwrap_or_else(|error| panic!("encode test marker: {error}"));
        write_atomic_text(
            &install_root.join(".moondesk-browser-install.json"),
            &marker_json,
        )
        .unwrap_or_else(|error| panic!("write test marker: {error}"));

        assert!(
            installed_from_root(&root, &manifest, "test-platform", &artifact)
                .unwrap_or_else(|error| panic!("verify test browser: {error}"))
                .is_some()
        );

        std::fs::write(&executable, b"tampered-browser")
            .unwrap_or_else(|error| panic!("tamper test browser: {error}"));
        assert!(
            installed_from_root(&root, &manifest, "test-platform", &artifact)
                .unwrap_or_else(|error| panic!("reverify test browser: {error}"))
                .is_none()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_targets_cannot_escape_browser_staging_root() {
        let root = Path::new("root");
        let base = root.join("a").join("b");
        assert!(lexical_path_within(root, &base, Path::new("../c")));
        assert!(lexical_path_within(root, &base, Path::new("../../c")));
        assert!(!lexical_path_within(
            root,
            &base,
            Path::new("../../../escape")
        ));
        assert!(!lexical_path_within(root, &base, Path::new("/absolute")));
    }
}
