use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::command;

pub const MAX_REGISTERED_WORKSPACES: usize = 32;
pub const MAX_WORKSPACE_NAME_CHARS: usize = 128;
pub const MAX_TOOL_INVOCATIONS_PER_WINDOW: usize = 600;
pub const TOOL_INVOCATION_RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_MCP_SLUG_CHARS: usize = 128;

pub fn generate_mcp_slug() -> String {
    let random = Uuid::new_v4();
    URL_SAFE_NO_PAD.encode(&random.as_bytes()[..12])
}

pub fn derive_workspace_name(root: &Path) -> String {
    root.file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(|value| value.chars().take(MAX_WORKSPACE_NAME_CHARS).collect())
        .unwrap_or_else(|| "Workspace".to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl AsRef<str>) -> Result<Self, String> {
        let value = value.as_ref().trim();
        let parsed =
            Uuid::parse_str(value).map_err(|_| format!("invalid workspace id: {value}"))?;
        Ok(Self(parsed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub fn test_default() -> Self {
        Self("00000000-0000-0000-0000-000000000001".to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        let normalized = Self::parse(&self.0)?;
        if normalized.0 != self.0 {
            return Err(format!(
                "workspace id must use canonical UUID form: {}",
                self.0
            ));
        }
        Ok(())
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
    pub mcp_slug: String,
}

impl WorkspaceConfig {
    pub fn new(
        name: impl AsRef<str>,
        root: impl AsRef<Path>,
        mcp_slug: impl AsRef<str>,
    ) -> Result<Self, String> {
        let name = normalize_workspace_name(name.as_ref())?;
        let root = canonicalize_existing_workspace_root(root.as_ref())?;
        let mcp_slug = mcp_slug.as_ref().trim().to_string();
        validate_mcp_slug(&mcp_slug)?;
        Ok(Self {
            id: WorkspaceId::new(),
            name,
            root,
            mcp_slug,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceAvailability {
    Available,
    #[default]
    Unavailable,
}

#[derive(Debug, Default)]
struct ToolInvocationWindow {
    started_at: Option<Instant>,
    count: usize,
}

#[derive(Debug)]
pub struct WorkspaceRuntime {
    accepting_requests: AtomicBool,
    in_flight_requests: AtomicUsize,
    remote_connected: AtomicBool,
    last_remote_activity_ms: AtomicU64,
    request_count: AtomicU64,
    tool_invocations: Mutex<ToolInvocationWindow>,
    drain_notify: Notify,
}

impl Default for WorkspaceRuntime {
    fn default() -> Self {
        Self {
            accepting_requests: AtomicBool::new(true),
            in_flight_requests: AtomicUsize::new(0),
            remote_connected: AtomicBool::new(false),
            last_remote_activity_ms: AtomicU64::new(0),
            request_count: AtomicU64::new(0),
            tool_invocations: Mutex::new(ToolInvocationWindow::default()),
            drain_notify: Notify::new(),
        }
    }
}

impl WorkspaceRuntime {
    pub fn try_acquire(self: &Arc<Self>) -> Option<WorkspaceRequestLease> {
        if !self.accepting_requests.load(Ordering::Acquire) {
            return None;
        }
        self.in_flight_requests.fetch_add(1, Ordering::AcqRel);
        if !self.accepting_requests.load(Ordering::Acquire) {
            self.release_request();
            return None;
        }
        Some(WorkspaceRequestLease {
            runtime: self.clone(),
        })
    }

    fn release_request(&self) {
        if self.in_flight_requests.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drain_notify.notify_waiters();
        }
    }

    pub fn revoke(&self) {
        self.accepting_requests.store(false, Ordering::Release);
        if self.in_flight_requests.load(Ordering::Acquire) == 0 {
            self.drain_notify.notify_waiters();
        }
    }

    pub fn enable(&self) {
        self.accepting_requests.store(true, Ordering::Release);
    }

    pub fn accepting_requests(&self) -> bool {
        self.accepting_requests.load(Ordering::Acquire)
    }

    pub fn in_flight_requests(&self) -> usize {
        self.in_flight_requests.load(Ordering::Acquire)
    }

    pub async fn wait_for_drain(&self) {
        loop {
            if self.in_flight_requests() == 0 {
                return;
            }
            let notified = self.drain_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight_requests() == 0 {
                return;
            }
            notified.await;
        }
    }

    pub fn set_remote_connected(&self, connected: bool) {
        self.remote_connected.store(connected, Ordering::Release);
        if !connected {
            self.last_remote_activity_ms.store(0, Ordering::Release);
        }
    }

    pub fn remote_connected(&self) -> bool {
        self.remote_connected.load(Ordering::Acquire)
    }

    pub fn mark_remote_activity(&self, timestamp_ms: u64) {
        self.last_remote_activity_ms
            .store(timestamp_ms, Ordering::Release);
        self.request_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn last_remote_activity_ms(&self) -> Option<u64> {
        match self.last_remote_activity_ms.load(Ordering::Acquire) {
            0 => None,
            value => Some(value),
        }
    }

    pub fn request_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }

    pub fn allow_tool_invocation(&self) -> bool {
        self.allow_tool_invocation_at(Instant::now())
    }

    fn allow_tool_invocation_at(&self, now: Instant) -> bool {
        let mut window = self
            .tool_invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = window.started_at.is_none_or(|started| {
            now.saturating_duration_since(started) >= TOOL_INVOCATION_RATE_WINDOW
        });
        if expired {
            window.started_at = Some(now);
            window.count = 0;
        }
        if window.count >= MAX_TOOL_INVOCATIONS_PER_WINDOW {
            return false;
        }
        window.count += 1;
        true
    }
}

#[derive(Debug)]
pub struct WorkspaceRequestLease {
    runtime: Arc<WorkspaceRuntime>,
}

impl Drop for WorkspaceRequestLease {
    fn drop(&mut self) {
        self.runtime.release_request();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRequestContext {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
}

impl From<&WorkspaceConfig> for WorkspaceRequestContext {
    fn from(workspace: &WorkspaceConfig) -> Self {
        Self {
            workspace_id: workspace.id.clone(),
            name: workspace.name.clone(),
            root: workspace.root.clone(),
        }
    }
}

pub fn normalize_workspace_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    if value.chars().any(char::is_control) {
        return Err("workspace name cannot contain control characters".into());
    }
    if value.chars().count() > MAX_WORKSPACE_NAME_CHARS {
        return Err(format!(
            "workspace name is too long; maximum is {MAX_WORKSPACE_NAME_CHARS} characters"
        ));
    }
    Ok(value.to_string())
}

pub fn validate_mcp_slug(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("workspace MCP slug cannot be empty".into());
    }
    if value.len() > MAX_MCP_SLUG_CHARS {
        return Err(format!(
            "workspace MCP slug is too long; maximum is {MAX_MCP_SLUG_CHARS} characters"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("workspace MCP slug must use only URL-safe letters, digits, '-' or '_'".into());
    }
    Ok(())
}

pub fn resolve_workspace_by_slug(
    workspaces: &[WorkspaceConfig],
    candidate: &str,
) -> Option<WorkspaceRequestContext> {
    let candidate = fixed_slug_bytes(candidate)?;
    let mut matched_index = None;
    for (index, workspace) in workspaces.iter().enumerate() {
        let Some(configured) = fixed_slug_bytes(&workspace.mcp_slug) else {
            continue;
        };
        let content_matches = configured.0.ct_eq(&candidate.0);
        let configured_len = (configured.1 as u64).to_le_bytes();
        let candidate_len = (candidate.1 as u64).to_le_bytes();
        let length_matches = configured_len.ct_eq(&candidate_len);
        if bool::from(content_matches & length_matches) {
            matched_index = Some(index);
        }
    }
    matched_index.map(|index| WorkspaceRequestContext::from(&workspaces[index]))
}

fn fixed_slug_bytes(value: &str) -> Option<([u8; MAX_MCP_SLUG_CHARS], usize)> {
    if value.len() > MAX_MCP_SLUG_CHARS {
        return None;
    }
    let mut padded = [0_u8; MAX_MCP_SLUG_CHARS];
    padded[..value.len()].copy_from_slice(value.as_bytes());
    Some((padded, value.len()))
}

pub fn canonicalize_existing_workspace_root(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!(
            "workspace root is not an available directory: {}",
            path.display()
        ));
    }
    path.canonicalize()
        .map(command::normalize_windows_verbatim_path)
        .map_err(|error| format!("failed to canonicalize workspace root: {error}"))
}

pub fn workspace_availability(root: &Path) -> WorkspaceAvailability {
    if !root.is_dir() {
        return WorkspaceAvailability::Unavailable;
    }
    match canonicalize_existing_workspace_root(root) {
        Ok(canonical) if comparable_root(&canonical) == comparable_root(root) => {
            WorkspaceAvailability::Available
        }
        _ => WorkspaceAvailability::Unavailable,
    }
}

pub fn validate_workspace_registry(workspaces: &[WorkspaceConfig]) -> Result<(), String> {
    if workspaces.len() > MAX_REGISTERED_WORKSPACES {
        return Err(format!(
            "too many registered workspaces: {} (maximum {MAX_REGISTERED_WORKSPACES})",
            workspaces.len()
        ));
    }

    let mut ids = HashSet::with_capacity(workspaces.len());
    let mut slugs = HashSet::with_capacity(workspaces.len());
    let mut normalized_roots = Vec::with_capacity(workspaces.len());

    for workspace in workspaces {
        workspace.id.validate()?;
        normalize_workspace_name(&workspace.name)?;
        validate_mcp_slug(&workspace.mcp_slug)?;
        validate_persisted_root(&workspace.root)?;

        if !ids.insert(workspace.id.clone()) {
            return Err(format!("duplicate workspace id: {}", workspace.id));
        }
        if !slugs.insert(workspace.mcp_slug.clone()) {
            return Err("duplicate workspace MCP slug".into());
        }

        let resolved = canonicalize_existing_workspace_root(&workspace.root)
            .unwrap_or_else(|_| workspace.root.clone());
        normalized_roots.push((
            workspace.name.as_str(),
            workspace.root.as_path(),
            comparable_root(&resolved),
        ));
    }

    for left in 0..normalized_roots.len() {
        for right in (left + 1)..normalized_roots.len() {
            let (left_name, left_root, left_key) = &normalized_roots[left];
            let (right_name, right_root, right_key) = &normalized_roots[right];
            if left_key == right_key || same_filesystem_object(left_root, right_root) {
                return Err(format!(
                    "duplicate workspace root: {} and {}",
                    left_root.display(),
                    right_root.display()
                ));
            }
            if left_key.starts_with(right_key)
                || right_key.starts_with(left_key)
                || filesystem_ancestor_of(left_root, right_root)
                || filesystem_ancestor_of(right_root, left_root)
            {
                return Err(format!(
                    "workspace roots must not overlap: {left_name} ({}) and {right_name} ({})",
                    left_root.display(),
                    right_root.display()
                ));
            }
        }
    }

    Ok(())
}

fn validate_persisted_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() {
        return Err(format!(
            "workspace root must be an absolute path: {}",
            root.display()
        ));
    }
    if root
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "workspace root must be stored in normalized form: {}",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn filesystem_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn same_filesystem_object(left: &Path, right: &Path) -> bool {
    matches!(
        (filesystem_identity(left), filesystem_identity(right)),
        (Some(left), Some(right)) if left == right
    )
}

// Existing roots are canonicalized before these identity fallbacks are used, so
// Windows junction aliases normally collapse to the same comparable path. On
// non-Unix targets we do not additionally compare stable file IDs; if an alias
// cannot be resolved by canonicalization, overlap detection falls back to the
// normalized `comparable_root` path checks.
#[cfg(not(unix))]
fn same_filesystem_object(_left: &Path, _right: &Path) -> bool {
    false
}

#[cfg(unix)]
fn filesystem_ancestor_of(ancestor: &Path, descendant: &Path) -> bool {
    let Some(ancestor_identity) = filesystem_identity(ancestor) else {
        return false;
    };
    descendant
        .ancestors()
        .skip(1)
        .any(|candidate| filesystem_identity(candidate) == Some(ancestor_identity))
}

// Same non-Unix fallback limitation as `same_filesystem_object` above.
#[cfg(not(unix))]
fn filesystem_ancestor_of(_ancestor: &Path, _descendant: &Path) -> bool {
    false
}

#[cfg(windows)]
fn comparable_root(root: &Path) -> PathBuf {
    PathBuf::from(
        root.to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase(),
    )
}

#[cfg(not(windows))]
fn comparable_root(root: &Path) -> PathBuf {
    root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("moondesk-workspace-{name}-{}", Uuid::new_v4()))
    }

    fn config(name: &str, root: PathBuf, slug: &str) -> WorkspaceConfig {
        WorkspaceConfig {
            id: WorkspaceId::new(),
            name: name.to_string(),
            root,
            mcp_slug: slug.to_string(),
        }
    }

    #[test]
    fn workspace_ids_are_uuid_backed_and_normalized() {
        let id = WorkspaceId::new();
        assert!(Uuid::parse_str(id.as_str()).is_ok());

        let upper = Uuid::new_v4().to_string().to_ascii_uppercase();
        let parsed = WorkspaceId::parse(&upper).expect("valid UUID should parse");
        assert_eq!(parsed.as_str(), upper.to_ascii_lowercase());
        assert!(WorkspaceId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn new_workspace_canonicalizes_existing_directory() {
        let root = absolute_test_root("canonical");
        std::fs::create_dir_all(&root).expect("create workspace root");
        let workspace = WorkspaceConfig::new("  SiteAI  ", &root, "Ab3kL9xQ2pTm7VhC")
            .expect("create workspace config");
        assert_eq!(workspace.name, "SiteAI");
        assert_eq!(
            workspace.root,
            command::normalize_windows_verbatim_path(
                root.canonicalize().expect("canonicalize expected root")
            )
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn new_workspace_rejects_file_as_root() {
        let root = absolute_test_root("file-root");
        std::fs::write(&root, "not a directory").expect("write temp file");
        assert!(WorkspaceConfig::new("Project", &root, "Ab3kL9xQ2pTm7VhC").is_err());
        let _ = std::fs::remove_file(root);
    }

    #[test]
    fn workspace_names_are_bounded_and_derived_names_are_valid() {
        let maximum = "x".repeat(MAX_WORKSPACE_NAME_CHARS);
        assert_eq!(
            normalize_workspace_name(&maximum).as_deref(),
            Ok(maximum.as_str())
        );

        let too_long = "x".repeat(MAX_WORKSPACE_NAME_CHARS + 1);
        assert!(normalize_workspace_name(&too_long).is_err());

        let long_root = PathBuf::from(format!(
            "root-{}",
            "y".repeat(MAX_WORKSPACE_NAME_CHARS + 20)
        ));
        let derived = derive_workspace_name(&long_root);
        assert_eq!(derived.chars().count(), MAX_WORKSPACE_NAME_CHARS);
        assert!(normalize_workspace_name(&derived).is_ok());
    }

    #[test]
    fn mcp_slug_validation_matches_current_url_safe_shape() {
        assert!(validate_mcp_slug("Ab3kL9xQ2pTm7VhC").is_ok());
        assert!(validate_mcp_slug("").is_err());
        assert!(validate_mcp_slug("contains/slash").is_err());
        assert!(validate_mcp_slug("contains space").is_err());
    }

    #[test]
    fn request_context_does_not_copy_secret_slug() {
        let workspace = config(
            "SiteAI",
            absolute_test_root("request-context"),
            "SecretSlug123456",
        );
        let context = WorkspaceRequestContext::from(&workspace);
        assert_eq!(context.workspace_id, workspace.id);
        assert_eq!(context.name, workspace.name);
        assert_eq!(context.root, workspace.root);
    }

    #[test]
    fn registry_rejects_duplicate_ids_slugs_and_roots() {
        let root_a = absolute_test_root("dup-a");
        let root_b = absolute_test_root("dup-b");
        let first = config("A", root_a.clone(), "SecretSlugAAAAAA");

        let mut duplicate_id = config("B", root_b.clone(), "SecretSlugBBBBBB");
        duplicate_id.id = first.id.clone();
        assert!(validate_workspace_registry(&[first.clone(), duplicate_id]).is_err());

        let duplicate_slug = config("B", root_b.clone(), &first.mcp_slug);
        assert!(validate_workspace_registry(&[first.clone(), duplicate_slug]).is_err());

        let duplicate_root = config("B", root_a, "SecretSlugCCCCCC");
        assert!(validate_workspace_registry(&[first, duplicate_root]).is_err());
    }

    #[test]
    fn registry_rejects_nested_workspace_roots() {
        let parent = absolute_test_root("parent");
        let child = parent.join("nested");
        let workspaces = [
            config("Parent", parent, "SecretSlugAAAAAA"),
            config("Child", child, "SecretSlugBBBBBB"),
        ];
        assert!(validate_workspace_registry(&workspaces).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn registry_loads_existing_symlink_root_alias_but_marks_it_unavailable() {
        use std::os::unix::fs::symlink;

        let target = absolute_test_root("persisted-symlink-target");
        let alias = absolute_test_root("persisted-symlink-alias");
        std::fs::create_dir_all(&target).expect("create symlink target");
        symlink(&target, &alias).expect("create workspace root symlink");
        let workspace = config("Alias", alias.clone(), "SecretSlugAAAAAA");

        assert!(validate_workspace_registry(&[workspace]).is_ok());
        assert_eq!(
            workspace_availability(&alias),
            WorkspaceAvailability::Unavailable,
            "a persisted alias must not silently retarget an existing connector"
        );

        let _ = std::fs::remove_file(alias);
        let _ = std::fs::remove_dir_all(target);
    }

    #[cfg(unix)]
    #[test]
    fn registry_rejects_two_roots_resolving_to_same_filesystem_directory() {
        use std::os::unix::fs::symlink;

        let target = absolute_test_root("filesystem-identity-target");
        let alias = absolute_test_root("filesystem-identity-alias");
        std::fs::create_dir_all(&target).expect("create target");
        symlink(&target, &alias).expect("create alias");
        let workspaces = [
            config("Target", target.clone(), "SecretSlugAAAAAA"),
            config("Alias", alias.clone(), "SecretSlugBBBBBB"),
        ];
        assert!(validate_workspace_registry(&workspaces).is_err());

        let _ = std::fs::remove_file(alias);
        let _ = std::fs::remove_dir_all(target);
    }

    #[test]
    fn registry_allows_unavailable_but_well_formed_absolute_root() {
        let root = absolute_test_root("offline");
        let workspace = config("Offline", root.clone(), "SecretSlugAAAAAA");
        assert_eq!(
            workspace_availability(&root),
            WorkspaceAvailability::Unavailable
        );
        assert!(validate_workspace_registry(&[workspace]).is_ok());
    }

    #[test]
    fn registry_enforces_workspace_count_without_dividing_runtime_quotas() {
        let workspaces = (0..=MAX_REGISTERED_WORKSPACES)
            .map(|index| {
                config(
                    &format!("Workspace {index}"),
                    absolute_test_root(&format!("count-{index}")),
                    &format!("WorkspaceSlug{index:03}"),
                )
            })
            .collect::<Vec<_>>();
        assert!(validate_workspace_registry(&workspaces).is_err());
    }

    #[test]
    fn disconnect_clears_workspace_remote_activity_timestamp() {
        let runtime = WorkspaceRuntime::default();
        runtime.set_remote_connected(true);
        runtime.mark_remote_activity(42);
        assert!(runtime.remote_connected());
        assert_eq!(runtime.last_remote_activity_ms(), Some(42));

        runtime.set_remote_connected(false);
        assert!(!runtime.remote_connected());
        assert_eq!(runtime.last_remote_activity_ms(), None);
    }

    #[test]
    fn tool_invocation_rate_limit_is_generous_per_workspace_and_resets() {
        let now = Instant::now();
        let workspace_a = WorkspaceRuntime::default();
        let workspace_b = WorkspaceRuntime::default();

        for _ in 0..MAX_TOOL_INVOCATIONS_PER_WINDOW {
            assert!(workspace_a.allow_tool_invocation_at(now));
        }
        assert!(!workspace_a.allow_tool_invocation_at(now));
        assert!(
            workspace_b.allow_tool_invocation_at(now),
            "one workspace must not consume another workspace's allowance"
        );
        assert!(
            workspace_a.allow_tool_invocation_at(now + TOOL_INVOCATION_RATE_WINDOW),
            "the fixed window must reset after its configured duration"
        );
    }
}
