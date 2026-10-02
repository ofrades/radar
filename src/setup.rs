//! Install radar's board gates into the user's global config — never a repo.
//!
//! Enabling a board wires, once per machine and idempotently:
//!   * the board skill at `~/.agents/skills/board/SKILL.md`;
//!   * an opencode plugin and an omp extension that call `radar hook guard`
//!     before a file-editing tool runs;
//!   * a `PreToolUse` hook merged into `~/.claude/settings.json`;
//!   * a git `core.hooksPath` dispatcher under radar's data dir that runs the
//!     commit gate and then chains any repo-local hook.
//!
//! Everything lives in `$HOME` or git's own local config, so nothing is added
//! to a repository's tracked tree. The guard itself fails open and only acts
//! for radar-launched agents in board-enabled projects, so a machine-wide
//! install is inert everywhere the user has not enabled a board.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// What was installed, for `radar setup` to report.
#[derive(Debug, Default)]
pub struct Installed {
    pub skill: Option<PathBuf>,
    pub opencode: Option<PathBuf>,
    pub omp: Option<PathBuf>,
    pub claude: Option<PathBuf>,
    pub session_hooks: Vec<PathBuf>,
    pub git_hooks_dir: Option<PathBuf>,
    pub git_hooks_path_set: bool,
}

const OPENCODE_PLUGIN: &str = r#"// radar board edit gate — managed by `radar setup`; safe to regenerate.
// Blocks a file-editing tool call from a radar-launched agent that holds no
// live board claim. Fails open on any error, and is inert for other agents.
import { execFileSync } from "node:child_process";

const GATE = "radar";
const EDIT_TOOLS = new Set([
  "edit", "write", "multiedit", "notebookedit", "patch", "apply_patch",
  "create_file", "str_replace",
]);

function pathOf(event) {
  const args = event?.args ?? event?.input ?? {};
  return (
    args.filePath || args.file_path || args.path || args.absolute_path ||
    event?.filePath || event?.file_path || ""
  );
}

function denial(path) {
  if (!path) return "";
  try {
    execFileSync(GATE, ["hook", "guard", "--file", path], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    return "";
  } catch (error) {
    const stderr = error?.stderr ? error.stderr.toString() : "";
    return (stderr || error?.message || "no board claim").trim();
  }
}

export default {
  id: "radar-board",
  async setup(ctx) {
    if (!ctx?.tool?.hook) return;
    await ctx.tool.hook("execute.before", (event) => {
      if (!EDIT_TOOLS.has(String(event?.tool ?? "").toLowerCase())) return;
      const reason = denial(pathOf(event));
      if (reason) throw new Error(reason);
    });
  },
};
"#;

const OMP_EXTENSION: &str = r#"// radar board edit gate — managed by `radar setup`; safe to regenerate.
// @ts-nocheck
import { execFileSync } from "node:child_process";

const EDIT_TOOLS = new Set([
  "edit", "write", "multiedit", "patch", "apply_patch", "create_file", "str_replace",
]);

function pathOf(event) {
  const args = event?.args ?? event?.input ?? {};
  return args.filePath || args.file_path || args.path || event?.filePath || "";
}

function denial(path) {
  if (!path) return "";
  try {
    execFileSync("radar", ["hook", "guard", "--file", path], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    return "";
  } catch (error) {
    return String(error?.stderr || error?.message || "no board claim").trim();
  }
}

export default function (pi) {
  if (!pi?.on) return;
  // Gate file-editing tool calls before they run. The exact hook name varies by
  // omp version; unsupported names simply never fire, so this stays safe.
  for (const name of ["tool_call", "tool.execute.before", "before_tool_call"]) {
    pi.on(name, (event) => {
      const tool = String(event?.tool ?? event?.name ?? "").toLowerCase();
      if (!EDIT_TOOLS.has(tool)) return;
      const reason = denial(pathOf(event));
      if (reason) throw new Error(reason);
    });
  }
}
"#;

const GIT_PRE_COMMIT: &str = r#"#!/bin/sh
# radar board gate — managed by `radar setup`; safe to regenerate.
# Runs the commit gate for a radar-launched agent, then chains the repository's
# own pre-commit hook if it has one. Fails open when radar is unavailable.
if [ -n "$RADAR_AGENT" ] && command -v radar >/dev/null 2>&1; then
  radar hook guard --commit || exit $?
fi
hooks_dir=$(git rev-parse --git-path hooks 2>/dev/null) || true
if [ -n "$hooks_dir" ]; then
  case "$hooks_dir" in
    /*) ;;
    *) hooks_dir="$(pwd)/$hooks_dir" ;;
  esac
  if [ -x "$hooks_dir/pre-commit" ] && [ "$hooks_dir/pre-commit" != "$0" ]; then
    exec "$hooks_dir/pre-commit" "$@"
  fi
fi
exit 0
"#;

/// Install everything into `home`. Idempotent; existing files are only rewritten
/// when their content differs, and `~/.claude/settings.json` is merged, never
/// clobbered.
pub fn install_global(home: &Path) -> Result<Installed> {
    let skill = crate::skill::install_global_skill(home)?;

    let opencode = home.join(".config/opencode/plugins/radar-board.js");
    write_if_changed(&opencode, OPENCODE_PLUGIN)?;

    let omp = home.join(".omp/agent/extensions/radar-board.ts");
    write_if_changed(&omp, OMP_EXTENSION)?;

    let claude = merge_claude_settings(&home.join(".claude/settings.json"))?;
    let session_hooks = install_session_hooks(home)?;

    let (git_hooks_dir, git_hooks_path_set) = install_git_hooks(home)?;

    Ok(Installed {
        skill: Some(skill),
        opencode: Some(opencode),
        omp: Some(omp),
        claude: Some(claude),
        session_hooks,
        git_hooks_dir: Some(git_hooks_dir),
        git_hooks_path_set,
    })
}

/// Session identity is required even when the project's board is disabled.
pub fn install_default_session_hooks() -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("RADAR_SKILLS_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .context("No home directory for provider integrations")?;
    install_session_hooks(&home)
}

fn install_session_hooks(home: &Path) -> Result<Vec<PathBuf>> {
    let pi = home.join(".pi/agent/extensions/radar-session.js");
    let omp = home.join(".omp/agent/extensions/radar-session.ts");
    let extension = include_str!("session/hooks/pi.js");
    write_if_changed(&pi, extension)?;
    write_if_changed(
        &omp,
        &extension.replace(
            "pi.on(\"session_start\", identify);",
            "pi.on(\"session_start\", identify);\n  pi.on(\"session_switch\", identify);",
        ),
    )?;
    let dir = home.join(".config/opencode/plugins/radar-session");
    write_if_changed(
        &dir.join("package.json"),
        r#"{"name":"radar-session","type":"module","exports":{".":"./index.js","./tui":"./tui.js"}}"#,
    )?;
    write_if_changed(
        &dir.join("index.js"),
        "export default { id: 'radar-session', setup() {} };\n",
    )?;
    let opencode = dir.join("tui.js");
    write_if_changed(&opencode, include_str!("session/hooks/opencode-tui.js"))?;
    let cursor = home.join(".cursor/radar-session.sh");
    write_if_changed(&cursor, include_str!("session/hooks/cursor.sh"))?;
    let settings = home.join(".cursor/hooks.json");
    let mut root: serde_json::Value = if settings.exists() {
        serde_json::from_str(&std::fs::read_to_string(&settings)?)?
    } else {
        serde_json::json!({"version": 1, "hooks": {}})
    };
    let hooks = root
        .as_object_mut()
        .context("Cursor settings must be an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    let command = format!(
        "sh '{}'",
        cursor.display().to_string().replace('\'', "'\\''")
    );
    for event in ["sessionStart", "beforeSubmitPrompt", "preToolUse"] {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .context("Cursor hook list must be an array")?;
        if !entries.iter().any(|entry| entry["command"] == command) {
            entries.push(serde_json::json!({"command":command,"timeout":10,"failClosed":true}));
        }
    }
    write_if_changed(
        &settings,
        &format!("{}\n", serde_json::to_string_pretty(&root)?),
    )?;
    Ok(vec![pi, omp, opencode, cursor, settings])
}

/// The default install: the current user's home.
pub fn install_default() -> Result<Installed> {
    let home = dirs::home_dir().context("no home directory for radar's global setup")?;
    install_global(&home)
}

/// Merge a `PreToolUse` gate into Claude Code's user settings, preserving
/// everything else and never adding a duplicate.
fn merge_claude_settings(path: &Path) -> Result<PathBuf> {
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).unwrap_or_default(),
        _ => serde_json::Value::Object(Default::default()),
    };
    let object = root
        .as_object_mut()
        .context("claude settings is not a JSON object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let pre = hooks
        .as_object_mut()
        .context("claude settings `hooks` is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    let entries = pre
        .as_array_mut()
        .context("claude settings `hooks.PreToolUse` is not an array")?;
    let present = entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|hooks| hooks.as_array())
            .is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command").and_then(|c| c.as_str()) == Some("radar hook guard")
                })
            })
    });
    if !present {
        entries.push(serde_json::json!({
            "matcher": "Edit|Write|MultiEdit|NotebookEdit",
            "hooks": [{ "type": "command", "command": "radar hook guard" }],
        }));
    }
    let text = serde_json::to_string_pretty(&root)?;
    write_if_changed(path, &format!("{text}\n"))?;
    Ok(path.to_path_buf())
}

/// Write radar's git gate dispatcher and point `core.hooksPath` at it, unless
/// the user already has a hooks path (then it is left untouched).
///
/// The git commands pin `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM` to `home`, so
/// they touch exactly `home/.gitconfig` and nothing ambient — the same file
/// git reads globally, and a temp file in tests.
fn install_git_hooks(home: &Path) -> Result<(PathBuf, bool)> {
    let dir = home.join(".local/share/radar/git-hooks");
    let hook = dir.join("pre-commit");
    write_if_changed(&hook, GIT_PRE_COMMIT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }

    let global = home.join(".gitconfig");
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
    };
    let set_to = run(&["config", "--global", "--get", "core.hooksPath"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if !set_to.is_empty() {
        // A pre-existing hooks path wins: do not clobber it.
        return Ok((dir, false));
    }
    let set = run(&[
        "config",
        "--global",
        "core.hooksPath",
        &dir.to_string_lossy(),
    ])
    .ok()
    .is_some_and(|output| output.status.success());
    Ok((dir, set))
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_global_writes_everything_outside_the_repo_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let installed = install_global(home.path()).unwrap();

        let skill = home.path().join(".agents/skills/board/SKILL.md");
        assert!(skill.is_file());
        assert!(installed.opencode.unwrap().is_file());
        assert!(installed.omp.unwrap().is_file());
        assert!(installed.claude.unwrap().is_file());

        let hook = installed.git_hooks_dir.unwrap().join("pre-commit");
        assert!(hook.is_file());
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(&hook).unwrap().permissions().mode() & 0o111,
            0,
            "the dispatcher must be executable"
        );

        // Running again changes nothing.
        let before = std::fs::read_to_string(&hook).unwrap();
        install_global(home.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), before);
    }

    #[test]
    fn claude_settings_are_merged_not_clobbered() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"enabledPlugins":{"x@y":true},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .unwrap();

        merge_claude_settings(&path).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["enabledPlugins"]["x@y"], true, "unrelated keys kept");
        let entries = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "our entry was appended, not replacing");
        assert!(entries.iter().any(|entry| entry["matcher"] == "Bash"));

        // A second run does not duplicate our entry.
        merge_claude_settings(&path).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }
}
