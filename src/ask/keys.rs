//! Where the API key lives: this process's environment, or the Windows user environment
//! (HKCU\Environment), which `ebira ask-setup` writes so that every tool on the computer can
//! read it, from Windows and from WSL alike. The key reaches PowerShell on its standard input,
//! never on a command line.

use std::io::Write as _;
use std::process::{Command, Stdio};

pub const VARIABLE: &str = "ANTHROPIC_API_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Process,
    WindowsUser,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Process => "this process's environment",
            Source::WindowsUser => "the Windows user environment",
        }
    }
}

/// The key from this process's environment, else the one saved in the Windows user environment.
pub fn find() -> Option<(String, Source)> {
    if let Ok(key) = std::env::var(VARIABLE) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some((key, Source::Process));
        }
    }
    read_user_variable(VARIABLE).map(|key| (key, Source::WindowsUser))
}

/// Why `find` found no key: where it looked, and what to do.
pub fn missing() -> String {
    missing_message(query_user_variable(VARIABLE).is_some())
}

fn missing_message(windows_readable: bool) -> String {
    if windows_readable {
        format!(
            "no API key: {VARIABLE} is not set in this process, and none is saved in the Windows \
             user environment; save one with `ebira ask-setup`"
        )
    } else {
        format!(
            "no API key: {VARIABLE} is not set in this process, and the Windows user environment, \
             where `ebira ask-setup` saves a key, cannot be read here; set {VARIABLE}"
        )
    }
}

/// A Windows user environment variable, read with reg.exe (from WSL through its Windows
/// interop); none when it is unset or Windows cannot be reached.
pub fn read_user_variable(name: &str) -> Option<String> {
    query_user_variable(name).flatten()
}

/// None when Windows cannot be reached (reg.exe does not run); otherwise the variable, if set.
pub fn query_user_variable(name: &str) -> Option<Option<String>> {
    let output = Command::new("reg.exe")
        .args(["query", "HKCU\\Environment", "/v", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    // reg.exe exits with 1 when the value does not exist.
    Some(if output.status.success() {
        parse_reg_query(&String::from_utf8_lossy(&output.stdout), name)
    } else {
        None
    })
}

fn parse_reg_query(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if !parts.next()?.eq_ignore_ascii_case(name) || !parts.next()?.starts_with("REG_") {
            return None;
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    })
}

/// Sets a Windows user environment variable; programs started afterwards see it.
pub fn set_user_variable(name: &str, value: &str) -> Result<(), String> {
    check_name(name)?;
    let script = format!(
        "$v = [Console]::In.ReadToEnd().Trim(); if (-not $v) {{ exit 3 }}; \
         [Environment]::SetEnvironmentVariable('{name}', $v, 'User')"
    );
    run_powershell(&script, value)
}

pub fn remove_user_variable(name: &str) -> Result<(), String> {
    check_name(name)?;
    run_powershell(
        &format!("[Environment]::SetEnvironmentVariable('{name}', $null, 'User')"),
        "",
    )
}

fn check_name(name: &str) -> Result<(), String> {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        Ok(())
    } else {
        Err(format!("unexpected variable name: {name}"))
    }
}

fn run_powershell(script: &str, input: &str) -> Result<(), String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &base64(&utf16),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!("cannot start powershell.exe (Windows, or WSL with Windows interop): {error}")
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|error| format!("cannot pass the value to powershell.exe: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("powershell.exe did not finish: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "the Windows user environment was not changed (exit code {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Whether the text looks like a key for the Messages API (not an Admin key), and why not.
pub fn check_format(key: &str) -> Result<(), &'static str> {
    if key.is_empty() {
        return Err("the key is empty");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "the key holds characters a key cannot have (spaces, quotes, or full-width letters?)",
        );
    }
    if key.starts_with("sk-ant-admin") {
        return Err(
            "this is an Admin key, which cannot ask questions; create an API key in the Console",
        );
    }
    if !key.starts_with("sk-ant-") || key.len() < 24 {
        return Err("this is not an API key (a long text that starts with sk-ant-)");
    }
    Ok(())
}

/// The key with its middle hidden: `sk-ant-api03-…AbCd`.
pub fn mask(key: &str) -> String {
    let count = key.chars().count();
    if count <= 17 {
        return "…".repeat(3);
    }
    let head: String = key.chars().take(13).collect();
    let tail: String = key.chars().skip(count - 4).collect();
    format!("{head}…{tail}")
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_key_names_both_places_looked_in() {
        let unsaved = missing_message(true);
        assert!(unsaved.starts_with("no API key: ANTHROPIC_API_KEY is not set in this process"));
        assert!(unsaved.contains("none is saved in the Windows user environment"));
        assert!(unsaved.ends_with("save one with `ebira ask-setup`"));
        let unreadable = missing_message(false);
        assert!(unreadable.contains("cannot be read here"));
        assert!(unreadable.ends_with("set ANTHROPIC_API_KEY"));
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn reads_reg_query_output() {
        let output =
            "\r\nHKEY_CURRENT_USER\\Environment\r\n    ANTHROPIC_API_KEY    REG_SZ    sk-ant-api03-abcdef\r\n\r\n";
        assert_eq!(
            parse_reg_query(output, "ANTHROPIC_API_KEY").as_deref(),
            Some("sk-ant-api03-abcdef")
        );
        assert_eq!(parse_reg_query(output, "OTHER"), None);
    }

    #[test]
    fn checks_key_shapes() {
        assert!(check_format("sk-ant-api03-0123456789abcdefABCDEF_-xyz").is_ok());
        assert!(check_format("").is_err());
        assert!(check_format("sk-ant-api03-012345678 9abcdef").is_err());
        assert!(check_format("sk-ant-admin01-0123456789abcdefABCDEF").is_err());
        assert!(check_format("sk-proj-0123456789abcdefABCDEF0123").is_err());
        assert!(check_format("ｓｋ-ant-api03-0123456789abcdef").is_err());
    }

    #[test]
    fn masks_the_middle() {
        assert_eq!(
            mask("sk-ant-api03-0123456789abcdefWXYZ"),
            "sk-ant-api03-…WXYZ"
        );
        assert_eq!(mask("short"), "………");
    }

    #[test]
    fn refuses_odd_variable_names() {
        assert!(check_name("ANTHROPIC_API_KEY").is_ok());
        assert!(check_name("X'); Remove-Item C:\\ #").is_err());
    }
}
