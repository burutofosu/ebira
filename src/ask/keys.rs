//! Where the API keys and the settings `ebira ask-setup` saves live: this process's environment,
//! or the Windows user environment (HKCU\Environment), which the page writes so that every tool
//! on the computer can read it, from Windows and from WSL alike. Values reach PowerShell on its
//! standard input, never on a command line.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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

/// The key in the environment variable `name`: this process's, else the Windows user one.
pub fn find_named(name: &str) -> Option<(String, Source)> {
    if let Ok(key) = std::env::var(name) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some((key, Source::Process));
        }
    }
    saved(name).map(|key| (key, Source::WindowsUser))
}

/// Why `find_named(name)` found no key: where it looked, and what to do.
pub fn missing_named(name: &str) -> String {
    missing_message(name, saved_values().is_some())
}

fn missing_message(name: &str, windows_readable: bool) -> String {
    if windows_readable {
        format!(
            "no API key: {name} is not set in this process, and none is saved in the Windows \
             user environment; save one with `ebira ask-setup`, or set {name}"
        )
    } else {
        format!(
            "no API key: {name} is not set in this process, and the Windows user environment \
             cannot be read here; set {name}"
        )
    }
}

/// A value saved in the Windows user environment, such as a setting `ebira ask-setup` saved.
/// One reg.exe call reads them all, the first time one is asked for.
pub fn saved(name: &str) -> Option<String> {
    lookup(saved_values()?, name)
}

fn saved_values() -> Option<&'static [(String, String)]> {
    static SAVED: OnceLock<Option<Vec<(String, String)>>> = OnceLock::new();
    SAVED.get_or_init(read_user_variables).as_deref()
}

/// The value of `name` among `values` (Windows names ignore case), if it holds more than spaces.
pub fn lookup(values: &[(String, String)], name: &str) -> Option<String> {
    values
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Every Windows user environment variable, read with one reg.exe call; none when Windows cannot
/// be reached.
pub fn read_user_variables() -> Option<Vec<(String, String)>> {
    let output = Command::new("reg.exe")
        .args(["query", "HKCU\\Environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_reg_values(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_reg_values(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            if !parts.next()?.starts_with("REG_") {
                return None;
            }
            let value = parts.collect::<Vec<_>>().join(" ");
            (!value.is_empty()).then(|| (name.to_string(), value))
        })
        .collect()
}

/// Sets (or, given none, removes) several Windows user environment variables with one
/// PowerShell call; the values reach it on its standard input. Programs started afterwards see
/// them.
pub fn set_user_variables(values: &[(&str, Option<&str>)]) -> Result<(), String> {
    let mut input = String::new();
    for (name, value) in values {
        check_name(name)?;
        let value = value.unwrap_or("").trim();
        if value.chars().any(char::is_control) {
            return Err(format!("{name} cannot hold line breaks or tabs"));
        }
        input.push_str(&format!("{name}\t{value}\n"));
    }
    run_powershell(
        "$text = [Console]::In.ReadToEnd(); \
         foreach ($line in $text -split \"`n\") { \
           $line = $line.TrimEnd(\"`r\"); if (-not $line) { continue }; \
           $parts = $line.Split(\"`t\", 2); \
           $value = if ($parts.Length -gt 1 -and $parts[1]) { $parts[1] } else { $null }; \
           [Environment]::SetEnvironmentVariable($parts[0], $value, 'User') }",
        &input,
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

/// Whether the text could be another service's key, and why not; a local server may take none.
pub fn check_other_format(key: &str, optional: bool) -> Result<(), &'static str> {
    if key.is_empty() {
        return if optional {
            Ok(())
        } else {
            Err("the key is empty")
        };
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "the key holds characters a key cannot have (spaces, quotes, or full-width letters?)",
        );
    }
    Ok(())
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

/// The key with its middle hidden: its prefix and its last four characters, such as
/// `sk-ant-api03-…AbCd` or `sk-proj-…AbCd`.
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 17 {
        return "…".repeat(3);
    }
    // The prefix runs to the last hyphen among the first 13 characters; without one, 4 are shown.
    let head = chars[..13]
        .iter()
        .rposition(|c| *c == '-')
        .map_or(4, |position| position + 1);
    let head: String = chars[..head].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
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
        let unsaved = missing_message(VARIABLE, true);
        assert!(unsaved.starts_with("no API key: ANTHROPIC_API_KEY is not set in this process"));
        assert!(unsaved.contains("none is saved in the Windows user environment"));
        assert!(unsaved.ends_with("save one with `ebira ask-setup`, or set ANTHROPIC_API_KEY"));
        let unreadable = missing_message(VARIABLE, false);
        assert!(unreadable.contains("cannot be read here"));
        assert!(unreadable.ends_with("set ANTHROPIC_API_KEY"));
        let other = missing_message("OPENAI_API_KEY", true);
        assert!(other.starts_with("no API key: OPENAI_API_KEY is not set in this process"));
        assert!(other.ends_with("save one with `ebira ask-setup`, or set OPENAI_API_KEY"));
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn reads_every_value_of_the_user_environment() {
        let output = "\r\nHKEY_CURRENT_USER\\Environment\r\n    EBIRA_ASK_API    REG_SZ    openai\r\n    \
                      Path    REG_EXPAND_SZ    C:\\Users\\h\\bin;C:\\Program Files\\x\r\n    EMPTY    REG_SZ    \r\n\r\n";
        assert_eq!(
            parse_reg_values(output),
            vec![
                ("EBIRA_ASK_API".to_string(), "openai".to_string()),
                (
                    "Path".to_string(),
                    "C:\\Users\\h\\bin;C:\\Program Files\\x".to_string()
                ),
            ]
        );
        let values = parse_reg_values(output);
        assert_eq!(lookup(&values, "ebira_ask_api").as_deref(), Some("openai"));
        assert_eq!(lookup(&values, "EMPTY"), None);
    }

    #[test]
    fn checks_other_keys_and_refuses_values_that_would_break_the_input() {
        assert!(check_other_format("sk-proj-abc_DEF-123", false).is_ok());
        assert!(
            check_other_format("", true).is_ok(),
            "a local server may take no key"
        );
        assert!(check_other_format("", false).is_err());
        assert!(check_other_format("sk proj", false).is_err());
        assert!(set_user_variables(&[("EBIRA_ASK_API", Some("open\nai"))]).is_err());
        assert!(set_user_variables(&[("lower_case", None)]).is_err());
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
        assert_eq!(mask("sk-proj-0123456789abcdefWXYZ"), "sk-proj-…WXYZ");
        assert_eq!(mask("AIzaSyA0123456789abcdefWXYZ"), "AIza…WXYZ");
        assert_eq!(mask("short"), "………");
    }

    #[test]
    fn refuses_odd_variable_names() {
        assert!(check_name("ANTHROPIC_API_KEY").is_ok());
        assert!(check_name("X'); Remove-Item C:\\ #").is_err());
    }
}
