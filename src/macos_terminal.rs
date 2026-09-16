#[cfg(target_os = "macos")]
use std::{
    env,
    ffi::CStr,
    os::raw::{c_char, c_int},
    process::Command,
};

#[cfg(target_os = "macos")]
const APPLE_TERMINAL_PROGRAM: &str = "Apple_Terminal";
#[cfg(target_os = "macos")]
const OSASCRIPT_PATH: &str = "/usr/bin/osascript";
#[cfg(any(target_os = "macos", test))]
const LEGACY_PROFILE_NAME: &str = "MoonDesk";
#[cfg(target_os = "macos")]
const SKIP_ENV: &str = "MOONDESK_SKIP_MACOS_TERMINAL_PROFILE";
#[cfg(target_os = "macos")]
const INSPECT_PROFILE_SCRIPT: &str = r#"
tell application "Terminal"
  set targetTTY to system attribute "MOONDESK_TERMINAL_TARGET_TTY"
  repeat with w in windows
    repeat with t in tabs of w
      try
        if tty of t is targetTTY then
          return name of current settings of t
        end if
      end try
    end repeat
  end repeat
  return ""
end tell
"#;
#[cfg(target_os = "macos")]
const RESTORE_DEFAULT_PROFILE_SCRIPT: &str = r#"
tell application "Terminal"
  set targetTTY to system attribute "MOONDESK_TERMINAL_TARGET_TTY"
  repeat with w in windows
    repeat with t in tabs of w
      try
        if tty of t is targetTTY then
          set current settings of t to default settings
          return true
        end if
      end try
    end repeat
  end repeat
  return false
end tell
"#;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn ttyname(fd: c_int) -> *const c_char;
}

/// Stop carrying forward MoonDesk's old Terminal.app profile.
///
/// Older releases imported a dedicated profile with compressed cell spacing and then applied it
/// to the active tab. That profile is no longer necessary and can distort block-glyph rendering.
/// If the current Terminal.app tab still uses that legacy profile, restore the user's own default
/// settings before the TUI enters raw/alternate-screen mode. This migration is deliberately
/// best-effort at the call site; Terminal automation permission must never become a startup
/// requirement for MoonDesk.
#[cfg(target_os = "macos")]
pub fn restore_legacy_profile_if_needed() -> Result<bool, String> {
    if !should_manage_terminal_launch() {
        return Ok(false);
    }

    let Some(current_tty) = current_tty() else {
        return Ok(false);
    };
    let profile_name = current_profile_name_for_tty(&current_tty)?;
    if !is_legacy_moondesk_profile(profile_name.as_deref()) {
        return Ok(false);
    }

    restore_default_profile_to_tty(&current_tty)
}

#[cfg(not(target_os = "macos"))]
pub fn restore_legacy_profile_if_needed() -> Result<bool, String> {
    Ok(false)
}

#[cfg(any(target_os = "macos", test))]
fn is_legacy_moondesk_profile(profile_name: Option<&str>) -> bool {
    profile_name == Some(LEGACY_PROFILE_NAME)
}

#[cfg(target_os = "macos")]
fn should_manage_terminal_launch() -> bool {
    env::var("TERM_PROGRAM").ok().as_deref() == Some(APPLE_TERMINAL_PROGRAM)
        && env::var_os(SKIP_ENV).is_none()
}

#[cfg(target_os = "macos")]
fn current_profile_name_for_tty(current_tty: &str) -> Result<Option<String>, String> {
    let profile_name = run_osascript_with_env(
        &[("MOONDESK_TERMINAL_TARGET_TTY", current_tty)],
        INSPECT_PROFILE_SCRIPT,
        "failed to inspect the current Terminal.app tab profile",
    )?
    .trim()
    .to_string();

    if profile_name.is_empty() {
        Ok(None)
    } else {
        Ok(Some(profile_name))
    }
}

#[cfg(target_os = "macos")]
fn restore_default_profile_to_tty(current_tty: &str) -> Result<bool, String> {
    run_osascript_with_env(
        &[("MOONDESK_TERMINAL_TARGET_TTY", current_tty)],
        RESTORE_DEFAULT_PROFILE_SCRIPT,
        "failed to restore the user's default Terminal.app profile",
    )
    .map(|stdout| stdout.trim() == "true")
}

#[cfg(target_os = "macos")]
fn current_tty() -> Option<String> {
    for fd in [0, 1, 2] {
        let tty_ptr = unsafe { ttyname(fd) };
        if tty_ptr.is_null() {
            continue;
        }
        let tty = unsafe { CStr::from_ptr(tty_ptr) }
            .to_string_lossy()
            .trim()
            .to_string();
        if !tty.is_empty() {
            return Some(tty);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn run_osascript_with_env(
    env_pairs: &[(&str, &str)],
    script: &str,
    context: &str,
) -> Result<String, String> {
    let mut command = Command::new(OSASCRIPT_PATH);
    for (key, value) in env_pairs {
        command.env(key, value);
    }

    let output = command
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|error| format!("{context}: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return if stderr.is_empty() {
            Err(format!("{context}: status {}", output.status))
        } else {
            Err(format!("{context}: {stderr}"))
        };
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::is_legacy_moondesk_profile;

    #[test]
    fn only_the_old_moondesk_profile_is_migrated() {
        assert!(is_legacy_moondesk_profile(Some("MoonDesk")));
        assert!(!is_legacy_moondesk_profile(Some("Basic")));
        assert!(!is_legacy_moondesk_profile(Some("Pro")));
        assert!(!is_legacy_moondesk_profile(None));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_terminal_migration_applescripts_compile() {
        use super::{INSPECT_PROFILE_SCRIPT, RESTORE_DEFAULT_PROFILE_SCRIPT};
        use std::{fs, process::Command};

        for (name, script) in [
            ("inspect", INSPECT_PROFILE_SCRIPT),
            ("restore", RESTORE_DEFAULT_PROFILE_SCRIPT),
        ] {
            let output_path = std::env::temp_dir().join(format!(
                "moondesk-{name}-terminal-profile-{}.scpt",
                std::process::id()
            ));
            let output = Command::new("/usr/bin/osacompile")
                .args(["-o", output_path.to_string_lossy().as_ref(), "-e", script])
                .output()
                .expect("run osacompile");
            let _ = fs::remove_file(&output_path);
            assert!(
                output.status.success(),
                "{name} AppleScript did not compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
