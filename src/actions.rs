//! Side effects triggered from the keyboard: focusing a Rex pane and copying
//! text to the clipboard.

use std::io::{self, Write as _};
use std::process::{Command, Stdio};

use base64::Engine as _;
use color_eyre::eyre::{Result, bail};

use crate::model::{Agent, RexTarget, Session};

/// Focuses the Rex pane hosting a session.
pub fn focus_rex(target: &RexTarget) -> Result<()> {
    let mut command = Command::new("rex");
    command.arg("focus");
    if let Some(session) = &target.session {
        command.args(["-s", session]);
    }
    let output = command
        .arg(&target.block)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| color_eyre::eyre::eyre!("cannot run rex: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "rex: {}",
            stderr.lines().next().unwrap_or("focus failed").trim()
        );
    }
    Ok(())
}

/// Deletes an exited session's conversation.
///
/// Codex and opencode delete through their own CLIs. Claude Code has no
/// command for a single conversation, so its transcript and the folder next
/// to it (tool results, sub-agents) are moved to the trash, where they can
/// be recovered.
pub fn delete_session(session: &Session) -> Result<()> {
    if session.agent != Agent::Claude && !crate::model::is_plain_id(&session.id) {
        bail!("unexpected session id {:?}", session.id);
    }
    match session.agent {
        Agent::Claude => {
            let Some(transcript) = &session.transcript else {
                bail!("transcript of {} not found", session.name);
            };
            let mut paths = vec![transcript.clone()];
            let folder = transcript.with_extension("");
            if folder.is_dir() {
                paths.push(folder);
            }
            move_to_trash(&paths)
        }
        Agent::Codex => run_cli("codex", &["delete", &session.id]),
        Agent::OpenCode => run_cli("opencode", &["session", "delete", &session.id]),
    }
}

/// Moves files to the trash. On macOS this uses the system file manager API
/// directly: going through the Finder would need an automation permission and
/// block the interface until it is granted.
fn move_to_trash(paths: &[std::path::PathBuf]) -> Result<()> {
    #[allow(unused_mut)]
    let mut trash = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos as _};
        trash.set_delete_method(DeleteMethod::NsFileManager);
    }
    trash
        .delete_all(paths)
        .map_err(|e| color_eyre::eyre::eyre!("cannot move to trash: {e}"))
}

fn run_cli(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| color_eyre::eyre::eyre!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{program}: {}",
            stderr.lines().next().unwrap_or("delete failed").trim()
        );
    }
    Ok(())
}

/// Copies `text` to the system clipboard.
///
/// Native clipboard tools are tried first; otherwise the terminal is asked to
/// do it with an OSC 52 escape sequence, which also works over SSH.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    const TOOLS: &[&[&str]] = &[
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
    ];
    for tool in TOOLS {
        let Ok(mut child) = Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        if child.wait()?.success() {
            return Ok(());
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}
