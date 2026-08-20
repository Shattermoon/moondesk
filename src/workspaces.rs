use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::command;

pub const MAX_REGISTERED_WORKSPACES: usize = 32;
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
        .map(str::to_string)
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceRuntime {
    pub availability: WorkspaceAvailability,
    pub accepting_requests: bool,
    pub in_flight_requests: usize,
    pub remote_connected: bool,
    pub last_remote_activity_ms: Option<u128>,
    pub request_count: u64,
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
    if root.is_dir() {
        WorkspaceAvailability::Available
    } else {
        WorkspaceAvailability::Unavailable
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
    let mut roots = HashSet::with_capacity(workspaces.len());
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

        let root_key = comparable_root(&workspace.root);
        if !roots.insert(root_key.clone()) {
            return Err(format!(
                "duplicate workspace root: {}",
                workspace.root.display()
            ));
        }
        normalized_roots.push((workspace.name.as_str(), workspace.root.as_path(), root_key));
    }

    for left in 0..normalized_roots.len() {
        for right in (left + 1)..normalized_roots.len() {
            let (left_name, left_root, left_key) = &normalized_roots[left];
            let (right_name, right_root, right_key) = &normalized_roots[right];
            if left_key.starts_with(right_key) || right_key.starts_with(left_key) {
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
}
