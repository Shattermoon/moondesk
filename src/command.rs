use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Node, Parser};
use tree_sitter_bash::LANGUAGE as BASH_LANGUAGE;

// `run_command` is one-shot, so keep the legacy capture budget rather than
// discarding output that cannot be recovered. Chatty commands should use the
// incremental start_command/poll_command path instead.
const MAX_BUFFER_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 120_000;
pub const MOONDESK_CO_AUTHOR_TRAILER: &str = "Co-Authored-By: MoonDesk";

fn raw_filesystem_delete_blocked_error() -> String {
    "code: RAW_FILESYSTEM_DELETE_BLOCKED\nmessage: MoonDesk blocks explicit shell deletion commands because shell quoting, variable expansion, absolute paths, and nested shells can escape the workspace. Use the dedicated `delete` tool for workspace-contained deletion, and split cleanup from any remaining shell command."
        .to_string()
}

fn destructive_disk_command_blocked_error() -> String {
    "code: DESTRUCTIVE_DISK_COMMAND_BLOCKED\nmessage: MoonDesk blocks disk/partition destructive commands in the generic developer shell. Run disk administration manually outside MoonDesk if you intentionally need it."
        .to_string()
}

fn safety_command_basename(word: &str) -> String {
    word.trim_matches(['\'', '"'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase()
}

fn is_raw_delete_command_name(word: &str) -> bool {
    let command = safety_command_basename(word);
    matches!(
        command.as_str(),
        "remove-item" | "rm" | "ri" | "rmdir" | "rd" | "del" | "erase" | "unlink"
    ) || command == "microsoft.powershell.managementremove-item"
}

fn is_disk_destructive_command_name(word: &str) -> bool {
    let command = safety_command_basename(word);
    matches!(
        command.as_str(),
        "format"
            | "format.com"
            | "diskpart"
            | "clear-disk"
            | "initialize-disk"
            | "remove-partition"
            | "wipefs"
            | "fdisk"
            | "parted"
    ) || command == "mkfs"
        || command.starts_with("mkfs.")
}

fn is_opaque_shell_evaluator_name(word: &str) -> bool {
    matches!(
        safety_command_basename(word).as_str(),
        "eval" | "iex" | "invoke-expression"
    )
}

fn opaque_shell_command_blocked_error() -> String {
    "code: OPAQUE_SHELL_COMMAND_BLOCKED\nmessage: MoonDesk blocks opaque shell evaluation because the executable payload cannot be inspected reliably. Run the intended concrete developer command directly instead."
        .to_string()
}

fn first_non_assignment_word(words: &[ShellWord]) -> Option<usize> {
    words
        .iter()
        .position(|word| !looks_like_env_assignment(&word.text))
}

fn nested_safety_shell_payload(
    words: &[ShellWord],
    command_idx: usize,
) -> Result<Option<&str>, String> {
    let Some(command_word) = words.get(command_idx) else {
        return Ok(None);
    };
    let command = safety_command_basename(&command_word.text);
    if matches!(command.as_str(), "bash" | "sh" | "zsh" | "dash") {
        return Ok(shell_command_arg_index(words, command_idx)
            .and_then(|idx| words.get(idx))
            .map(|word| word.text.as_str()));
    }

    if matches!(command.as_str(), "cmd" | "cmd.exe") {
        for idx in command_idx + 1..words.len() {
            if matches!(words[idx].lower.as_str(), "/c" | "/k") {
                return Ok(words.get(idx + 1).map(|word| word.text.as_str()));
            }
        }
        return Ok(None);
    }

    if matches!(
        command.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        for idx in command_idx + 1..words.len() {
            match words[idx].lower.as_str() {
                "-command" | "-c" => {
                    return Ok(words.get(idx + 1).map(|word| word.text.as_str()));
                }
                "-encodedcommand" | "-enc" | "-e" => {
                    return Err(
                        "code: OPAQUE_SHELL_COMMAND_BLOCKED\nmessage: MoonDesk blocks encoded shell command payloads because their filesystem effects cannot be inspected safely."
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
    }

    Ok(None)
}

fn unsupported_wrapper_syntax_error(wrapper: &str, option: &str) -> String {
    format!(
        "code: OPAQUE_WRAPPED_COMMAND_BLOCKED\nmessage: MoonDesk could not safely identify the executable behind `{wrapper}` option `{option}`. Run the concrete developer command directly instead."
    )
}

fn option_has_attached_value(word: &str, short: char) -> bool {
    word.starts_with('-')
        && !word.starts_with("--")
        && word.chars().nth(1) == Some(short)
        && word.len() > 2
}

fn wrapper_command_index(
    wrapper: &str,
    words: &[ShellWord],
    start: usize,
) -> Result<Option<usize>, String> {
    let mut idx = start;
    while idx < words.len() {
        let word = words[idx].text.as_str();
        let lower = words[idx].lower.as_str();

        if word == "--" {
            return Ok(words.get(idx + 1).map(|_| idx + 1));
        }

        if wrapper == "env" && looks_like_env_assignment(word) {
            idx += 1;
            continue;
        }

        if !word.starts_with('-') || word == "-" {
            return Ok(Some(idx));
        }

        let short_option = word
            .strip_prefix('-')
            .filter(|value| !value.starts_with('-'))
            .and_then(|value| value.chars().next());
        let long_takes_value = |options: &[&str]| {
            options
                .iter()
                .any(|option| lower == *option || lower.starts_with(&format!("{option}=")))
        };
        let takes_value = match wrapper {
            "sudo" => {
                short_option.is_some_and(|option| {
                    ['C', 'D', 'g', 'h', 'p', 'R', 'T', 'U', 'u', 'r', 't'].contains(&option)
                }) || long_takes_value(&[
                    "--close-from",
                    "--chdir",
                    "--group",
                    "--host",
                    "--prompt",
                    "--chroot",
                    "--command-timeout",
                    "--user",
                    "--role",
                    "--type",
                ])
            }
            "env" => {
                short_option.is_some_and(|option| ['C', 'S', 'u'].contains(&option))
                    || long_takes_value(&["--chdir", "--split-string", "--unset"])
            }
            "exec" => short_option == Some('a'),
            _ => false,
        };

        let known_flag = match wrapper {
            "sudo" => matches!(
                lower,
                "-a" | "-b"
                    | "-e"
                    | "-h"
                    | "-k"
                    | "-n"
                    | "-p"
                    | "-s"
                    | "-v"
                    | "--askpass"
                    | "--background"
                    | "--preserve-env"
                    | "--help"
                    | "--reset-timestamp"
                    | "--non-interactive"
                    | "--stdin"
                    | "--validate"
            ),
            "env" => matches!(
                lower,
                "-0" | "--null"
                    | "-i"
                    | "--ignore-environment"
                    | "--debug"
                    | "--help"
                    | "--version"
            ),
            "nohup" => matches!(lower, "--help" | "--version"),
            "exec" => matches!(lower, "-c" | "-l"),
            "builtin" => false,
            "command" => matches!(lower, "-p"),
            _ => false,
        };

        if takes_value {
            let short = lower.chars().nth(1);
            let attached = lower.contains('=')
                || short.is_some_and(|short| option_has_attached_value(lower, short));
            idx += if attached { 1 } else { 2 };
            continue;
        }
        if known_flag {
            idx += 1;
            continue;
        }

        return Err(unsupported_wrapper_syntax_error(wrapper, word));
    }
    Ok(None)
}

fn xargs_command_index(words: &[ShellWord], start: usize) -> Result<Option<usize>, String> {
    let mut idx = start;
    while idx < words.len() {
        let word = words[idx].text.as_str();
        let lower = words[idx].lower.as_str();
        if word == "--" {
            return Ok(words.get(idx + 1).map(|_| idx + 1));
        }
        if !word.starts_with('-') || word == "-" {
            return Ok(Some(idx));
        }

        let value_short = ['a', 'd', 'E', 'I', 'L', 'n', 'P', 's'];
        let takes_short_value = word
            .chars()
            .nth(1)
            .is_some_and(|ch| value_short.contains(&ch));
        let takes_long_value = [
            "--arg-file",
            "--delimiter",
            "--eof",
            "--replace",
            "--max-lines",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--process-slot-var",
        ]
        .iter()
        .any(|option| lower == *option || lower.starts_with(&format!("{option}=")));
        if takes_short_value || takes_long_value {
            let attached = lower.contains('=') || (takes_short_value && word.len() > 2);
            idx += if attached { 1 } else { 2 };
            continue;
        }

        if matches!(
            lower,
            "-0" | "--null"
                | "-o"
                | "--open-tty"
                | "-p"
                | "--interactive"
                | "-r"
                | "--no-run-if-empty"
                | "-t"
                | "--verbose"
                | "-x"
                | "--exit"
                | "--show-limits"
                | "--help"
                | "--version"
        ) {
            idx += 1;
            continue;
        }

        return Err(unsupported_wrapper_syntax_error("xargs", word));
    }
    Ok(None)
}

fn validate_command_at(words: &[ShellWord], start: usize, depth: usize) -> Result<(), String> {
    if start < words.len() {
        validate_parsed_command_words(&words[start..], depth)?;
    }
    Ok(())
}

fn validate_parsed_command_words(words: &[ShellWord], depth: usize) -> Result<(), String> {
    let Some(command_idx) = first_non_assignment_word(words) else {
        return Ok(());
    };
    let command = safety_command_basename(&words[command_idx].text);

    if is_raw_delete_command_name(&command) {
        return Err(raw_filesystem_delete_blocked_error());
    }
    if is_disk_destructive_command_name(&command) {
        return Err(destructive_disk_command_blocked_error());
    }
    if is_opaque_shell_evaluator_name(&command) {
        return Err(opaque_shell_command_blocked_error());
    }

    if command == "diskutil"
        && words
            .iter()
            .skip(command_idx + 1)
            .any(|word| word.lower.starts_with("erase"))
    {
        return Err(destructive_disk_command_blocked_error());
    }

    if matches!(
        command.as_str(),
        "sudo" | "env" | "builtin" | "exec" | "nohup"
    ) {
        if let Some(wrapped_idx) = wrapper_command_index(&command, words, command_idx + 1)? {
            validate_command_at(words, wrapped_idx, depth)?;
        }
    } else if command == "command" {
        let lookup_only = words
            .iter()
            .skip(command_idx + 1)
            .take_while(|word| word.text.starts_with('-'))
            .any(|word| matches!(word.text.as_str(), "-v" | "-V"));
        if !lookup_only
            && let Some(wrapped_idx) = wrapper_command_index("command", words, command_idx + 1)?
        {
            validate_command_at(words, wrapped_idx, depth)?;
        }
    }

    if command == "find" {
        let mut idx = command_idx + 1;
        while idx < words.len() {
            if words[idx].lower == "-delete" {
                return Err(raw_filesystem_delete_blocked_error());
            }
            if matches!(
                words[idx].lower.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir"
            ) {
                validate_command_at(words, idx + 1, depth)?;
                idx += 2;
                while idx < words.len() && !matches!(words[idx].text.as_str(), ";" | "+") {
                    idx += 1;
                }
            }
            idx += 1;
        }
    }

    if command == "xargs"
        && let Some(wrapped_idx) = xargs_command_index(words, command_idx + 1)?
    {
        validate_command_at(words, wrapped_idx, depth)?;
    }

    if let Some(payload) = nested_safety_shell_payload(words, command_idx)? {
        validate_parsed_shell_command_contexts(payload, depth + 1)?;
    }

    Ok(())
}

fn validate_parsed_shell_command_contexts(command: &str, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err(
            "code: SHELL_COMMAND_NESTING_BLOCKED\nmessage: MoonDesk refused an excessively nested shell command because it could not safely establish its filesystem effects."
                .to_string(),
        );
    }

    let Some(tree) = parse_shell(command) else {
        return Ok(());
    };
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            let text = node
                .utf8_text(command.as_bytes())
                .map_err(|error| {
                    format!(
                        "code: COMMAND_SAFETY_PARSE_FAILED\nmessage: MoonDesk could not inspect a parsed shell command safely: {error}"
                    )
                })?;
            validate_parsed_command_words(&shell_words(text), depth)?;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    Ok(())
}

/// Reject destructive filesystem primitives that would bypass MoonDesk's
/// workspace-contained file tools. Normal developer commands remain available;
/// explicit deletion must go through the dedicated `delete` tool instead.
pub fn validate_shell_command_safety(command: &str) -> Result<(), String> {
    validate_parsed_shell_command_contexts(command, 0)
}

#[derive(Debug)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub output_archive_truncated: bool,
    pub output_archive_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileListingFilter {
    All,
    FilesOnly,
    DirectoriesOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListFilesInterceptSource {
    Find,
    Tree,
    Ls,
    Rg,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterceptedListFilesRequest {
    pub source: ListFilesInterceptSource,
    pub path: Option<String>,
    pub include_hidden: bool,
    pub filter: FileListingFilter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterceptedMovePathRequest {
    pub from: String,
    pub to: String,
    pub overwrite: bool,
}

/// Clamp timeout to [1, MAX_TIMEOUT_MS].
pub fn clamp_timeout(t: Option<u64>) -> u64 {
    match t {
        Some(v) if v >= 1 => v.min(MAX_TIMEOUT_MS),
        _ => DEFAULT_TIMEOUT_MS,
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    normalized
}

fn resolve_contained_path(
    workspace_root: &str,
    base: &Path,
    input: &str,
) -> Result<PathBuf, String> {
    let root = Path::new(workspace_root)
        .canonicalize()
        .map(normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())?;
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        base.join(input)
    };
    let candidate = normalize_lexically(&candidate);

    // Canonicalize the nearest existing ancestor rather than the whole candidate.
    // New leaves cannot be canonicalized, and a lexical prefix check would accept
    // paths such as `workspace/../outside/file`. Resolving the ancestor also catches
    // symlink/junction escapes before any missing suffix components are appended.
    let mut ancestor = candidate.clone();
    let mut missing_suffix: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().map(OsString::from).ok_or_else(|| {
                    format!(
                        "Unable to resolve path inside workspace: {}",
                        candidate.display()
                    )
                })?;
                missing_suffix.push(name);
                if !ancestor.pop() {
                    return Err(format!(
                        "Unable to resolve path inside workspace: {}",
                        candidate.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "Unable to inspect path while resolving inside workspace ({}): {error}",
                    ancestor.display()
                ));
            }
        }
    }

    let canonical_ancestor = ancestor
        .canonicalize()
        .map(normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())?;
    if !canonical_ancestor.starts_with(&root) {
        return Err(format!(
            "Path escapes workspace root: {}",
            candidate.display()
        ));
    }

    let mut resolved = canonical_ancestor;
    for component in missing_suffix.iter().rev() {
        resolved.push(component);
    }
    if !resolved.starts_with(&root) {
        return Err(format!(
            "Path escapes workspace root: {}",
            candidate.display()
        ));
    }
    Ok(resolved)
}

/// Resolve `input` relative to `workspace_root`, rejecting traversal and symlink escapes.
pub fn resolve_workspace_path(
    workspace_root: &str,
    input: Option<&str>,
) -> Result<PathBuf, String> {
    let root = Path::new(workspace_root)
        .canonicalize()
        .map(normalize_windows_verbatim_path)
        .map_err(|e| e.to_string())?;
    resolve_contained_path(workspace_root, &root, input.unwrap_or("."))
}

/// Resolve `input` relative to `cwd`, rejecting traversal and symlink escapes.
pub fn resolve_command_path(
    workspace_root: &str,
    cwd: &Path,
    input: Option<&str>,
) -> Result<PathBuf, String> {
    resolve_contained_path(workspace_root, cwd, input.unwrap_or("."))
}

pub fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    normalize_windows_verbatim_path_impl(path)
}

#[cfg(windows)]
fn normalize_windows_verbatim_path_impl(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };

    let mut normalized = match prefix.kind() {
        Prefix::VerbatimDisk(disk) => PathBuf::from(format!("{}:\\", disk as char)),
        Prefix::VerbatimUNC(server, share) => PathBuf::from(format!(
            r"\\{}\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        )),
        _ => return path,
    };

    for component in components {
        if matches!(component, Component::RootDir) {
            continue;
        }
        normalized.push(component.as_os_str());
    }

    normalized
}

#[cfg(not(windows))]
fn normalize_windows_verbatim_path_impl(path: PathBuf) -> PathBuf {
    path
}

pub fn detect_list_files_intercept(command: &str) -> Option<InterceptedListFilesRequest> {
    let words = parse_word_only_shell_command(command)?;
    detect_list_files_intercept_from_words(&words)
}

pub fn detect_move_path_intercept(command: &str) -> Option<InterceptedMovePathRequest> {
    let words = parse_word_only_shell_command(command)?;
    detect_move_path_intercept_from_words(&words)
}

/// Execute a short shell command via MoonDesk's shared process runner.
///
/// The process runner owns the complete process tree. If this future is timed
/// out or dropped because the MCP request disappears, the child tree is
/// terminated instead of being left behind as an orphaned build.
#[cfg(test)]
pub async fn run_command(command: &str, cwd: &Path, timeout_ms: u64) -> CommandResult {
    run_command_archived(command, cwd, timeout_ms, None).await
}

pub async fn run_command_archived(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    output_paths: Option<&crate::process_runner::CommandOutputPaths>,
) -> CommandResult {
    let result = crate::process_runner::run_shell_command(
        command,
        cwd,
        timeout_ms,
        MAX_BUFFER_BYTES,
        output_paths,
    )
    .await;

    CommandResult {
        stdout: result.stdout,
        stderr: result.stderr,
        success: result.success,
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        stdout_truncated: result.stdout_truncated,
        stderr_truncated: result.stderr_truncated,
        output_archive_truncated: result.output_archive_truncated,
        output_archive_error: result.output_archive_error,
    }
}

pub fn contains_moondesk_co_author_marker(command: &str) -> bool {
    let haystack = command.to_ascii_lowercase();
    let mut cursor = 0usize;
    for needle in ["co", "author", "by", "moondesk"] {
        let Some(offset) = haystack[cursor..].find(needle) else {
            return false;
        };
        cursor += offset + needle.len();
    }
    true
}

pub fn command_contains_git_commit(command: &str) -> bool {
    shell_segments(command)
        .iter()
        .any(|segment| segment_contains_git_commit(segment))
}

pub fn inject_moondesk_co_author_trailer(command: &str) -> String {
    let mut rewritten = String::with_capacity(command.len() + 64);
    for segment in shell_segments(command) {
        rewritten.push_str(&inject_trailer_into_segment(&segment));
    }
    rewritten
}

fn segment_contains_git_commit(segment: &str) -> bool {
    if git_commit_insert_pos(segment).is_some() {
        return true;
    }
    nested_shell_command(segment)
        .map(|payload| command_contains_git_commit(&payload.command))
        .unwrap_or(false)
}

fn inject_trailer_into_segment(segment: &str) -> String {
    if let Some(insert_pos) = git_commit_insert_pos(segment) {
        let mut rewritten = String::with_capacity(segment.len() + 48);
        rewritten.push_str(&segment[..insert_pos]);
        rewritten.push_str(" --trailer '");
        rewritten.push_str(MOONDESK_CO_AUTHOR_TRAILER);
        rewritten.push('\'');
        rewritten.push_str(&segment[insert_pos..]);
        return rewritten;
    }

    let Some(payload) = nested_shell_command(segment) else {
        return segment.to_string();
    };
    let rewritten_command = inject_moondesk_co_author_trailer(&payload.command);
    if rewritten_command == payload.command {
        return segment.to_string();
    }

    let mut rewritten = String::with_capacity(segment.len() + 64);
    rewritten.push_str(&segment[..payload.start]);
    rewritten.push_str(&shell_single_quote(&rewritten_command));
    rewritten.push_str(&segment[payload.end..]);
    rewritten
}

fn git_commit_insert_pos(segment: &str) -> Option<usize> {
    let words = shell_words(segment);
    let git_idx = command_start_git_index(&words)?;
    let commit_word = words[git_idx + 1..]
        .iter()
        .find(|word| word.lower == "commit")?;
    Some(commit_word.end)
}

fn detect_list_files_intercept_from_words(words: &[String]) -> Option<InterceptedListFilesRequest> {
    let command_idx = command_start_word_index(words)?;
    let command = words.get(command_idx)?;
    if is_shell_command(command) {
        let nested_idx = shell_command_arg_word_index(words, command_idx)?;
        let nested_command = words.get(nested_idx)?;
        return detect_list_files_intercept(nested_command);
    }

    match command_basename(command).as_str() {
        "find" => parse_find_list_files_args(&words[command_idx + 1..]),
        "tree" => parse_tree_list_files_args(&words[command_idx + 1..]),
        "ls" => parse_ls_list_files_args(&words[command_idx + 1..]),
        "rg" => parse_rg_list_files_args(&words[command_idx + 1..]),
        _ => None,
    }
}

fn detect_move_path_intercept_from_words(words: &[String]) -> Option<InterceptedMovePathRequest> {
    let command_idx = command_start_word_index(words)?;
    let command = words.get(command_idx)?;
    if is_shell_command(command) {
        let nested_idx = shell_command_arg_word_index(words, command_idx)?;
        let nested_command = words.get(nested_idx)?;
        return detect_move_path_intercept(nested_command);
    }

    match command_basename(command).as_str() {
        "mv" => parse_mv_move_path_args(&words[command_idx + 1..]),
        _ => None,
    }
}

fn command_start_word_index(words: &[String]) -> Option<usize> {
    let mut idx = 0usize;
    while idx < words.len() && looks_like_env_assignment(&words[idx]) {
        idx += 1;
    }
    loop {
        let word = words.get(idx)?;
        let lower = word.to_ascii_lowercase();
        match lower.as_str() {
            "sudo" => {
                idx += 1;
                while idx < words.len() && words[idx].starts_with('-') {
                    idx += 1;
                }
            }
            "env" => {
                idx += 1;
                while idx < words.len()
                    && (words[idx].starts_with('-') || looks_like_env_assignment(&words[idx]))
                {
                    idx += 1;
                }
            }
            _ => break,
        }
    }
    words.get(idx).map(|_| idx)
}

fn shell_command_arg_word_index(words: &[String], shell_idx: usize) -> Option<usize> {
    let mut idx = shell_idx + 1;
    while idx < words.len() {
        let word = words[idx].to_ascii_lowercase();
        if word == "--" {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if word == "-c" {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if word.starts_with('-')
            && word.len() > 2
            && word[1..].chars().all(|ch| matches!(ch, 'c' | 'l'))
            && word[1..].contains('c')
        {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if !word.starts_with('-') {
            return None;
        }
        idx += 1;
    }
    None
}

fn parse_find_list_files_args(args: &[String]) -> Option<InterceptedListFilesRequest> {
    let (path, remainder) = match args.first() {
        Some(arg) if !is_find_expression_token(arg) => (Some(arg.clone()), &args[1..]),
        _ => (None, args),
    };

    let filter = match remainder {
        [] => FileListingFilter::All,
        [flag, kind] if flag == "-type" => match kind.as_str() {
            "f" => FileListingFilter::FilesOnly,
            "d" => FileListingFilter::DirectoriesOnly,
            _ => return None,
        },
        _ => return None,
    };

    Some(InterceptedListFilesRequest {
        source: ListFilesInterceptSource::Find,
        path,
        include_hidden: true,
        filter,
    })
}

fn parse_tree_list_files_args(args: &[String]) -> Option<InterceptedListFilesRequest> {
    let mut path = None;
    let mut include_hidden = false;

    for arg in args {
        match arg.as_str() {
            "-a" | "--all" => include_hidden = true,
            value if value.starts_with('-') => return None,
            value => {
                if path.is_some() {
                    return None;
                }
                path = Some(value.to_string());
            }
        }
    }

    Some(InterceptedListFilesRequest {
        source: ListFilesInterceptSource::Tree,
        path,
        include_hidden,
        filter: FileListingFilter::All,
    })
}

fn parse_ls_list_files_args(args: &[String]) -> Option<InterceptedListFilesRequest> {
    let mut path = None;
    let mut include_hidden = false;
    let mut recursive = false;

    for arg in args {
        match arg.as_str() {
            "--recursive" => recursive = true,
            "--all" | "--almost-all" => include_hidden = true,
            value if value.starts_with("--") => return None,
            value if value.starts_with('-') => {
                for ch in value[1..].chars() {
                    match ch {
                        'R' => recursive = true,
                        'a' | 'A' => include_hidden = true,
                        _ => return None,
                    }
                }
            }
            value => {
                if path.is_some() {
                    return None;
                }
                path = Some(value.to_string());
            }
        }
    }

    if !recursive {
        return None;
    }

    Some(InterceptedListFilesRequest {
        source: ListFilesInterceptSource::Ls,
        path,
        include_hidden,
        filter: FileListingFilter::All,
    })
}

fn parse_rg_list_files_args(args: &[String]) -> Option<InterceptedListFilesRequest> {
    let mut path = None;
    let mut include_hidden = false;
    let mut files_only = false;
    let mut treat_next_as_path = false;

    for arg in args {
        if treat_next_as_path {
            if path.is_some() {
                return None;
            }
            path = Some(arg.clone());
            treat_next_as_path = false;
            continue;
        }

        match arg.as_str() {
            "--files" => files_only = true,
            "--hidden" => include_hidden = true,
            "--" => treat_next_as_path = true,
            value if value.starts_with('-') => return None,
            value => {
                if path.is_some() {
                    return None;
                }
                path = Some(value.to_string());
            }
        }
    }

    if !files_only || treat_next_as_path {
        return None;
    }

    Some(InterceptedListFilesRequest {
        source: ListFilesInterceptSource::Rg,
        path,
        include_hidden,
        filter: FileListingFilter::FilesOnly,
    })
}

fn parse_mv_move_path_args(args: &[String]) -> Option<InterceptedMovePathRequest> {
    let mut operands: Vec<String> = Vec::new();
    let mut overwrite = true;
    let mut parse_options = true;

    for arg in args {
        if parse_options && arg == "--" {
            parse_options = false;
            continue;
        }

        if parse_options && arg.starts_with("--") {
            match arg.as_str() {
                "--force" => overwrite = true,
                "--no-clobber" => overwrite = false,
                _ => return None,
            }
            continue;
        }

        if parse_options && arg.starts_with('-') && arg != "-" {
            for ch in arg[1..].chars() {
                match ch {
                    'f' => overwrite = true,
                    'n' => overwrite = false,
                    _ => return None,
                }
            }
            continue;
        }

        operands.push(arg.clone());
    }

    match operands.as_slice() {
        [from, to] => Some(InterceptedMovePathRequest {
            from: from.clone(),
            to: to.clone(),
            overwrite,
        }),
        _ => None,
    }
}

fn is_find_expression_token(word: &str) -> bool {
    word.starts_with('-') || matches!(word, "!" | "(" | ")")
}

fn command_basename(word: &str) -> String {
    word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase()
}

#[derive(Clone)]
struct ShellWord {
    text: String,
    lower: String,
    start: usize,
    end: usize,
}

struct NestedShellCommand {
    command: String,
    start: usize,
    end: usize,
}

fn shell_words(segment: &str) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start: Option<usize> = None;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (idx, ch) in segment.char_indices() {
        if escaped {
            if start.is_none() {
                start = Some(idx);
            }
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            if start.is_none() {
                start = Some(idx);
            }
            if cfg!(windows) {
                // MoonDesk uses PowerShell as the host shell on Windows. In
                // PowerShell a backslash is a path separator, not an escape
                // character, so preserve it for executable classification.
                current.push(ch);
            } else {
                escaped = true;
            }
            continue;
        }
        if ch == '\'' && !in_double {
            if start.is_none() {
                start = Some(idx);
            }
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            if start.is_none() {
                start = Some(idx);
            }
            in_double = !in_double;
            continue;
        }
        if !in_single && !in_double && ch.is_whitespace() {
            if let Some(word_start) = start {
                words.push(ShellWord {
                    lower: current.to_ascii_lowercase(),
                    text: current.clone(),
                    start: word_start,
                    end: idx,
                });
                current.clear();
                start = None;
            }
            continue;
        }
        if start.is_none() {
            start = Some(idx);
        }
        current.push(ch);
    }

    if let Some(word_start) = start {
        words.push(ShellWord {
            lower: current.to_ascii_lowercase(),
            text: current.clone(),
            start: word_start,
            end: segment.len(),
        });
    }

    words
}

fn nested_shell_command(segment: &str) -> Option<NestedShellCommand> {
    let words = shell_words(segment);
    let shell_idx = command_start_shell_index(&words)?;
    let command_idx = shell_command_arg_index(&words, shell_idx)?;
    let payload = words.get(command_idx)?;
    Some(NestedShellCommand {
        command: payload.text.clone(),
        start: payload.start,
        end: payload.end,
    })
}

fn command_start_git_index(words: &[ShellWord]) -> Option<usize> {
    command_start_index(words, |word| word == "git")
}

fn command_start_shell_index(words: &[ShellWord]) -> Option<usize> {
    command_start_index(words, is_shell_command)
}

fn command_start_index<F>(words: &[ShellWord], matches_command: F) -> Option<usize>
where
    F: Fn(&str) -> bool,
{
    let mut idx = 0usize;
    while idx < words.len() && looks_like_env_assignment(&words[idx].text) {
        idx += 1;
    }
    loop {
        let word = words.get(idx)?;
        match word.lower.as_str() {
            "sudo" => {
                idx += 1;
                while idx < words.len() && words[idx].text.starts_with('-') {
                    idx += 1;
                }
            }
            "env" => {
                idx += 1;
                while idx < words.len()
                    && (words[idx].text.starts_with('-')
                        || looks_like_env_assignment(&words[idx].text))
                {
                    idx += 1;
                }
            }
            _ => break,
        }
    }
    words
        .get(idx)
        .filter(|word| matches_command(&word.lower))
        .map(|_| idx)
}

fn is_shell_command(word: &str) -> bool {
    matches!(
        word.rsplit('/').next().unwrap_or(word),
        "bash" | "sh" | "zsh" | "dash"
    )
}

fn parse_word_only_shell_command(command: &str) -> Option<Vec<String>> {
    let tree = parse_shell(command)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    const ALLOWED_KINDS: &[&str] = &[
        "program",
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
    ];
    const ALLOWED_PUNCTUATION: &[&str] = &["\"", "'"];

    let mut stack = vec![root];
    let mut command_node = None;
    while let Some(node) = stack.pop() {
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&node.kind()) {
                return None;
            }
            if node.kind() == "command" {
                if command_node.is_some() {
                    return None;
                }
                command_node = Some(node);
            }
        } else if !(node.kind().trim().is_empty() || ALLOWED_PUNCTUATION.contains(&node.kind())) {
            return None;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    parse_plain_command_from_node(command_node?, command)
}

fn parse_shell(command: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&BASH_LANGUAGE.into()).ok()?;
    parser.parse(command, None)
}

fn parse_plain_command_from_node(node: Node<'_>, src: &str) -> Option<Vec<String>> {
    if node.kind() != "command" {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if !matches!(word_node.kind(), "word" | "number") {
                    return None;
                }
                words.push(word_node.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "word" | "number" => {
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "string" => words.push(parse_double_quoted_string(child, src)?),
            "raw_string" => words.push(parse_raw_string(child, src)?),
            "concatenation" => {
                let mut combined = String::new();
                let mut concat_cursor = child.walk();
                for part in child.named_children(&mut concat_cursor) {
                    match part.kind() {
                        "word" | "number" => {
                            combined.push_str(part.utf8_text(src.as_bytes()).ok()?);
                        }
                        "string" => combined.push_str(&parse_double_quoted_string(part, src)?),
                        "raw_string" => combined.push_str(&parse_raw_string(part, src)?),
                        _ => return None,
                    }
                }
                if combined.is_empty() {
                    return None;
                }
                words.push(combined);
            }
            _ => return None,
        }
    }

    Some(words)
}

fn parse_double_quoted_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "string_content" {
            return None;
        }
    }

    node.utf8_text(src.as_bytes())
        .ok()?
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .map(str::to_owned)
}

fn parse_raw_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "raw_string" {
        return None;
    }

    node.utf8_text(src.as_bytes())
        .ok()?
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .map(str::to_owned)
}

fn shell_command_arg_index(words: &[ShellWord], shell_idx: usize) -> Option<usize> {
    let mut idx = shell_idx + 1;
    while idx < words.len() {
        let word = &words[idx].lower;
        if word == "--" {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if word == "-c" {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if word.starts_with('-')
            && word.len() > 2
            && word[1..].chars().all(|ch| matches!(ch, 'c' | 'l'))
            && word[1..].contains('c')
        {
            return words.get(idx + 1).map(|_| idx + 1);
        }
        if !word.starts_with('-') {
            return None;
        }
        idx += 1;
    }
    None
}

fn looks_like_env_assignment(word: &str) -> bool {
    let Some((name, _value)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn shell_single_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if in_single || in_double {
            continue;
        }

        let separator_len = match ch {
            ';' | '\n' => Some(1usize),
            '&' => {
                if matches!(chars.peek(), Some((_, '&'))) {
                    chars.next();
                    Some(2usize)
                } else {
                    Some(1usize)
                }
            }
            '|' => {
                if matches!(chars.peek(), Some((_, '|'))) {
                    chars.next();
                    Some(2usize)
                } else {
                    Some(1usize)
                }
            }
            _ => None,
        };

        if let Some(separator_len) = separator_len {
            segments.push(command[start..idx + separator_len].to_string());
            start = idx + separator_len;
        }
    }

    if start < command.len() {
        segments.push(command[start..].to_string());
    }

    if segments.is_empty() {
        segments.push(String::new());
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("moondesk-command-{name}-{}", Uuid::new_v4()))
    }

    #[test]
    fn shell_safety_blocks_incident_style_nested_cmd_recursive_delete() {
        let command = r#"$ErrorActionPreference='Stop'; $boundary=(Resolve-Path '.worktrees/boundary' -ErrorAction SilentlyContinue); if($boundary){ Write-Output 'Deleting detached boundary residual'; cmd /c "rmdir /s /q \\"$($boundary.Path)\\""; if(Test-Path '.worktrees/boundary'){ Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force -ErrorAction Stop } }; git worktree prune"#;
        let error = validate_shell_command_safety(command)
            .expect_err("incident-style recursive delete must be blocked");
        assert!(error.contains("RAW_FILESYSTEM_DELETE_BLOCKED"));
    }

    #[test]
    fn shell_safety_blocks_direct_delete_primitives_across_shells() {
        for command in [
            "Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "rm -rf .worktrees/boundary",
            "rmdir /s /q .worktrees\\boundary",
            "& 'Remove-Item' -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "Microsoft.PowerShell.Management\\Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "bash -lc 'rm -rf .worktrees/boundary'",
            "find .worktrees -type f -delete",
            "printf '%s\\n' stale | xargs rm -f",
        ] {
            let error = validate_shell_command_safety(command)
                .expect_err("raw delete primitive must be blocked");
            assert!(
                error.contains("RAW_FILESYSTEM_DELETE_BLOCKED"),
                "unexpected error for {command}: {error}"
            );
        }
    }

    #[test]
    fn shell_safety_blocks_compound_subexpression_and_exec_bypasses() {
        for command in [
            "if true; then rm -rf protected; fi",
            "find protected -exec rm -rf {} +",
            "find protected -exec sh -c 'rm -rf \"$1\"' _ {} +",
            "Write-Output $(Remove-Item -Recurse -Force protected)",
            "bash -lc 'if true; then rm -rf protected; fi'",
            "command rm -rf protected",
            "sudo -u root rm -rf protected",
            "sudo --user=root rm -rf protected",
            "env TARGET=protected rm -rf protected",
            "nohup rm -rf protected",
            "exec rm -rf protected",
            "command -p rm -rf protected",
            "powershell -EncodedCommand ZABlAGwA",
            "eval 'rm -rf protected'",
            "Invoke-Expression 'Remove-Item -Recurse -Force protected'",
        ] {
            let error = validate_shell_command_safety(command)
                .expect_err("compound or indirect destructive command must be blocked");
            assert!(
                error.contains("RAW_FILESYSTEM_DELETE_BLOCKED")
                    || error.contains("OPAQUE_SHELL_COMMAND_BLOCKED"),
                "unexpected error for {command}: {error}"
            );
        }
    }

    #[test]
    fn parsed_context_guard_catches_existing_destructive_cases_without_raw_text_scan() {
        for command in [
            "Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "rm -rf .worktrees/boundary",
            "rmdir /s /q .worktrees\\boundary",
            "& 'Remove-Item' -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "Microsoft.PowerShell.Management\\Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force",
            "bash -lc 'rm -rf .worktrees/boundary'",
            "cmd /c \"rmdir /s /q .worktrees\\boundary\"",
            "powershell -Command \"Remove-Item -LiteralPath '.worktrees/boundary' -Recurse -Force\"",
            "find .worktrees -type f -delete",
            "printf '%s\\n' stale | xargs rm -f",
            "if true; then rm -rf protected; fi",
            "find protected -exec rm -rf {} +",
            "find protected -exec sh -c 'rm -rf \\\"$1\\\"' _ {} +",
            "find protected -exec cat {} + -delete",
            "Write-Output $(Remove-Item -Recurse -Force protected)",
            "powershell -EncodedCommand ZABlAGwA",
            "eval 'rm -rf protected'",
            "Invoke-Expression 'Remove-Item -Recurse -Force protected'",
            r#"& C:\Windows\System32\cmd.exe /c "rmdir /s /q C:\outside""#,
            r#"& "C:\Windows\System32\cmd.exe" /c "rmdir /s /q C:\outside""#,
            r#"& $env:SystemRoot\System32\cmd.exe /c "rmdir /s /q C:\outside""#,
            r#"& \\localhost\C$\Windows\System32\cmd.exe /c "rmdir /s /q C:\outside""#,
            "format D: /Q /Y",
            "Clear-Disk -Number 2 -RemoveData -Confirm:$false",
            "sudo mkfs.ext4 /dev/sdb1",
            "diskutil eraseDisk APFS Scratch /dev/disk4",
        ] {
            assert!(
                validate_parsed_shell_command_contexts(command, 0).is_err(),
                "parsed context guard should reject: {command}"
            );
        }
    }

    #[test]
    fn shell_safety_blocks_disk_destructive_commands() {
        for command in [
            "format D: /Q /Y",
            "Clear-Disk -Number 2 -RemoveData -Confirm:$false",
            "sudo mkfs.ext4 /dev/sdb1",
            "diskutil eraseDisk APFS Scratch /dev/disk4",
        ] {
            let error = validate_shell_command_safety(command)
                .expect_err("disk destructive command must be blocked");
            assert!(
                error.contains("DESTRUCTIVE_DISK_COMMAND_BLOCKED"),
                "unexpected error for {command}: {error}"
            );
        }
    }

    #[test]
    fn shell_safety_keeps_normal_developer_commands_available() {
        for command in [
            "git status --short --branch",
            "cargo test --all-targets",
            "npm ci && npm test",
            "git clean -ndx",
            "Write-Output 'rm -rf is blocked by MoonDesk'",
            "printf '%s\\n' 'rm -rf protected'",
            "command -v rm",
            "rg -n \"Remove-Item|rm -rf\" src",
            "git grep -nE \"rm|rmdir|Remove-Item\" -- src",
            "grep -R -E \"rm|rmdir|Remove-Item\" src",
            "Get-ChildItem -Recurse -File | Select-String -Pattern 'rm|rmdir|Remove-Item'",
            "Get-ChildItem -Recurse -File | ForEach-Object { Get-Content $_.FullName }",
            "find . -type f -exec cat {} +",
            "find . -type f -print0 | xargs -0 cat",
            "rg --files",
            "tree /f",
            "sudo rg -n \"rm\" src",
            "sudo -u nobody rg -n \"rm\" src",
            "sudo -H rg -n \"rm\" src",
            "sudo --user=nobody rg -n \"rm\" src",
            "env PATTERN=rm rg -n \"rm\" src",
            "find . -type f -exec rg -n \"rm\" {} +",
            "find . -type f -exec rg -n \"-delete\" {} +",
            "printf '%s\\n' src | xargs rg -n \"rm\"",
        ] {
            assert!(
                validate_shell_command_safety(command).is_ok(),
                "normal developer command should remain available: {command}"
            );
        }
    }

    #[test]
    fn resolve_workspace_path_defaults_to_workspace_root_for_missing_or_dot_cwd() {
        let workspace_root = test_workspace("resolve-default");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let expected = normalize_windows_verbatim_path(
            workspace_root
                .canonicalize()
                .expect("canonicalize workspace"),
        );

        assert_eq!(
            resolve_workspace_path(&workspace_root_str, None).expect("resolve missing cwd"),
            expected
        );
        assert_eq!(
            resolve_workspace_path(&workspace_root_str, Some(".")).expect("resolve dot cwd"),
            expected
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn resolve_workspace_path_rejects_nonexistent_parent_escape() {
        let workspace_root = test_workspace("resolve-parent-escape");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        let outside_name = format!(
            "{}-outside",
            workspace_root
                .file_name()
                .expect("workspace leaf")
                .to_string_lossy()
        );
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();
        let escaped = format!("../{outside_name}/new-file.txt");

        let error = resolve_workspace_path(&workspace_root_str, Some(&escaped))
            .expect_err("nonexistent parent traversal must be rejected");
        assert!(error.contains("escapes workspace root"));

        let safe = resolve_workspace_path(&workspace_root_str, Some("new/subdirectory/file.txt"))
            .expect("nonexistent path inside workspace should resolve");
        assert!(
            safe.starts_with(normalize_windows_verbatim_path(
                workspace_root
                    .canonicalize()
                    .expect("canonicalize workspace")
            ))
        );

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_workspace_path_rejects_current_drive_root_and_drive_root() {
        let workspace_root = test_workspace("resolve-drive-root-escape");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        for escaped in ["\\", "\\."] {
            let error = resolve_workspace_path(&workspace_root_str, Some(escaped))
                .expect_err("current-drive root path must be rejected");
            assert!(error.contains("escapes workspace root"));
        }

        let drive_root = workspace_root
            .components()
            .next()
            .and_then(|component| match component {
                Component::Prefix(prefix) => Some(PathBuf::from(format!(
                    "{}\\",
                    prefix.as_os_str().to_string_lossy()
                ))),
                _ => None,
            })
            .expect("test workspace should be on a Windows drive");
        let error =
            resolve_workspace_path(&workspace_root_str, Some(&drive_root.to_string_lossy()))
                .expect_err("drive root path must be rejected");
        assert!(error.contains("escapes workspace root"));

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_rejects_symlink_ancestor_escape() {
        use std::os::unix::fs::symlink;

        let workspace_root = test_workspace("resolve-symlink-escape");
        let outside_root = test_workspace("resolve-symlink-outside");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&outside_root).expect("create outside");
        symlink(&outside_root, workspace_root.join("outside-link")).expect("create symlink");
        let workspace_root_str = workspace_root.to_string_lossy().into_owned();

        let error = resolve_workspace_path(&workspace_root_str, Some("outside-link/new-file.txt"))
            .expect_err("symlink ancestor escape must be rejected");
        assert!(error.contains("escapes workspace root"));

        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[tokio::test]
    async fn run_command_uses_platform_shell_and_cwd() {
        let workspace_root = test_workspace("run-cwd");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        let leaf = workspace_root
            .file_name()
            .expect("workspace leaf")
            .to_string_lossy()
            .into_owned();
        let command = if cfg!(windows) {
            "Split-Path -Leaf (Get-Location).Path"
        } else {
            "basename \"$PWD\""
        };

        let result = run_command(command, &workspace_root, 10_000).await;

        assert!(result.success, "stderr: {}", result.stderr);
        assert_eq!(result.stdout.trim(), leaf);

        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn contains_moondesk_co_author_marker_matches_spaced_and_punctuated_phrase() {
        assert!(contains_moondesk_co_author_marker(
            "git commit -m \"Fix bug\\n\\nCo-Authored-By: MoonDesk\""
        ));
        assert!(contains_moondesk_co_author_marker(
            "git commit -m \"co***author___by:::moondesk\""
        ));
        assert!(!contains_moondesk_co_author_marker(
            "git commit -m \"fix bug\""
        ));
    }

    #[test]
    fn inject_moondesk_co_author_trailer_rewrites_each_git_commit_segment() {
        let rewritten =
            inject_moondesk_co_author_trailer("git add . && git commit -m \"test\" && git status");
        assert_eq!(
            rewritten,
            "git add . && git commit --trailer 'Co-Authored-By: MoonDesk' -m \"test\" && git status"
        );
    }

    #[test]
    fn inject_moondesk_co_author_trailer_rewrites_nested_shell_commit_commands() {
        let rewritten = inject_moondesk_co_author_trailer(
            "bash -lc 'git add src/mcp.rs && git commit -m \"Update MCP metadata handling\"'",
        );
        assert_eq!(
            rewritten,
            "bash -lc 'git add src/mcp.rs && git commit --trailer '\"'\"'Co-Authored-By: MoonDesk'\"'\"' -m \"Update MCP metadata handling\"'"
        );
    }

    #[test]
    fn command_contains_git_commit_only_matches_real_commit_tokens() {
        assert!(command_contains_git_commit("git commit -m \"x\""));
        assert!(command_contains_git_commit(
            "FOO=1 git -C repo commit -m \"x\""
        ));
        assert!(command_contains_git_commit(
            "bash -lc 'git commit -m \"x\"'"
        ));
        assert!(!command_contains_git_commit("echo git commit"));
    }

    #[test]
    fn detect_list_files_intercept_for_plain_find_command() {
        assert_eq!(
            detect_list_files_intercept("find src"),
            Some(InterceptedListFilesRequest {
                source: ListFilesInterceptSource::Find,
                path: Some("src".into()),
                include_hidden: true,
                filter: FileListingFilter::All,
            })
        );
    }

    #[test]
    fn detect_list_files_intercept_for_nested_shell_rg_files() {
        assert_eq!(
            detect_list_files_intercept("bash -lc 'rg --files --hidden src'"),
            Some(InterceptedListFilesRequest {
                source: ListFilesInterceptSource::Rg,
                path: Some("src".into()),
                include_hidden: true,
                filter: FileListingFilter::FilesOnly,
            })
        );
    }

    #[test]
    fn detect_list_files_intercept_for_ls_recursive() {
        assert_eq!(
            detect_list_files_intercept("ls -Ra src"),
            Some(InterceptedListFilesRequest {
                source: ListFilesInterceptSource::Ls,
                path: Some("src".into()),
                include_hidden: true,
                filter: FileListingFilter::All,
            })
        );
    }

    #[test]
    fn detect_list_files_intercept_rejects_complex_find_expression() {
        assert_eq!(detect_list_files_intercept("find . -name '*.rs'"), None);
    }

    #[test]
    fn detect_move_path_intercept_for_plain_mv_command() {
        assert_eq!(
            detect_move_path_intercept("mv src/old.txt src/new.txt"),
            Some(InterceptedMovePathRequest {
                from: "src/old.txt".into(),
                to: "src/new.txt".into(),
                overwrite: true,
            })
        );
    }

    #[test]
    fn detect_move_path_intercept_for_nested_no_clobber_mv_command() {
        assert_eq!(
            detect_move_path_intercept("bash -lc 'mv -n src/old.txt src/new.txt'"),
            Some(InterceptedMovePathRequest {
                from: "src/old.txt".into(),
                to: "src/new.txt".into(),
                overwrite: false,
            })
        );
    }

    #[test]
    fn detect_move_path_intercept_rejects_multi_source_or_unsupported_flags() {
        assert_eq!(detect_move_path_intercept("mv a b c"), None);
        assert_eq!(detect_move_path_intercept("mv -r a b"), None);
    }
}
