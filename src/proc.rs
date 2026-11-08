//! Spawning agent CLIs and MCP servers on Windows.
//!
//! npm-installed tools are `.cmd` shims, which CreateProcess cannot start directly.
//! Those go through `cmd.exe /d /c`. Only configured arguments travel that way;
//! prompts always go through stdin so user text never reaches a shell.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::Stdio;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn resolve(cmd: &str) -> Result<PathBuf> {
    let expanded = crate::config::expand(cmd, std::path::Path::new(""), std::path::Path::new(""));
    if expanded.components().count() > 1 {
        return if expanded.exists() {
            Ok(expanded)
        } else {
            Err(anyhow!("{} not found", expanded.display()))
        };
    }
    which::which(&expanded).map_err(|_| anyhow!("'{cmd}' not found on PATH"))
}

fn is_shim(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("cmd") | Some("bat")
    )
}

/// Program and argument list that start `cmd` with `args`, with shims wrapped in cmd.exe.
pub fn launch_spec(cmd: &str, args: &[String]) -> Result<(String, Vec<String>)> {
    let path = resolve(cmd)?;
    let path_s = path.to_string_lossy().to_string();
    if is_shim(&path) {
        for a in args {
            if a.contains(['&', '|', '<', '>', '^', '%', '"', '\n']) {
                return Err(anyhow!("argument '{a}' is unsafe to pass through cmd.exe"));
            }
        }
        let mut v = vec!["/d".to_string(), "/c".to_string(), path_s];
        v.extend(args.iter().cloned());
        Ok(("cmd.exe".to_string(), v))
    } else {
        Ok((path_s, args.to_vec()))
    }
}

pub fn command(cmd: &str, args: &[String]) -> Result<tokio::process::Command> {
    let (program, argv) = launch_spec(cmd, args)?;
    let mut c = tokio::process::Command::new(program);
    c.args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shell_metacharacters_for_shims() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("x.cmd");
        std::fs::write(&shim, "@echo off").unwrap();
        let s = shim.to_string_lossy().to_string();
        assert!(launch_spec(&s, &["ok".into()]).is_ok());
        assert!(launch_spec(&s, &["a&b".into()]).is_err());
        let (prog, argv) = launch_spec(&s, &["ok".into()]).unwrap();
        assert_eq!(prog, "cmd.exe");
        assert_eq!(&argv[..2], &["/d".to_string(), "/c".to_string()]);
    }
}
