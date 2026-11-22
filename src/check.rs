//! Sandboxed Lean project verification using `lake check` and `lake comparator`.
//!
//! `lake check` builds a project, replays it through the Lean kernel, and rejects axioms outside
//! `propext`, `Classical.choice`, and `Quot.sound`. `lake comparator` additionally checks proved
//! statements against a challenge specification. Both run untrusted project code inside bubblewrap
//! on Linux, or inside a Linux Docker container on other platforms.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::claim::Verdict;
use crate::files;

/// Sandbox runner used when building and checking a Lean project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// Use bubblewrap on Linux, or Docker otherwise.
    #[default]
    Auto,
    /// Run Lake's built-in bubblewrap sandbox directly on the host.
    Bwrap,
    /// Run Lake's bubblewrap sandbox inside a Linux Docker container.
    Docker,
    /// Disable sandboxing. Executes project code with host privileges; trusted projects only.
    None,
}

/// Verdict produced by a completed checker run. Infrastructure and build errors return `Err`
/// instead of an outcome so no claim is signed.
pub struct Outcome {
    pub checker: String,
    pub toolchain: String,
    pub runner: String,
    pub result: Verdict,
    pub exit_code: i32,
    pub detail: Option<String>,
}

/// Output markers emitted by Lake 4.36 when rejecting a proof (kernel error, disallowed axiom,
/// or challenge mismatch). Exit code 1 alone is ambiguous because sandbox startup failures and
/// out-of-memory errors also exit with 1. Once Lean 4.37+ JSON verdicts are required, this list
/// can be removed.
const REJECTIONS: [&str; 9] = [
    "kernel rejected the solution",
    "is not permitted; it is used by",
    "Illegal axiom detected",
    "do not match",
    "does not match",
    "don't match",
    "not found in solution",
    "not found in challenge",
    "is not a theorem",
];

/// Remedy for Ubuntu-based Docker VMs that block unprivileged user namespaces needed by bubblewrap.
const USERNS_HINT: &str = "bubblewrap could not create user namespaces. On an Ubuntu-based Docker VM \
such as colima, enable them with:\n  colima ssh -- sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0";

const IMAGE: &str = "delphi-lean-check:3";
/// Docker volume name for caching downloaded Lean toolchains across runs.
const TOOLCHAINS: &str = "delphi-elan";
const DOCKERFILE: &str = include_str!("../docker/lean-check.Dockerfile");

/// Runs the Lean checker on the project in `dir`.
///
/// `comparator` is an optional relative path to a comparator config inside `dir`.
/// Setting `network` enables network access in the container so Lake can fetch package
/// dependencies (such as Mathlib), which also grants project build code network access.
pub async fn run(dir: &Path, comparator: Option<&str>, runner: Runner, network: bool) -> Result<Outcome> {
    let toolchain = toolchain(dir)?;
    if !network && has_dependencies(dir)? {
        bail!("project has external dependencies that require network access; re-run with --network");
    }
    let args: Vec<&str> = match comparator {
        Some(config) => {
            // Validate remote claim path before passing it as a CLI argument.
            files::checked(config)?;
            vec!["comparator", "--config", config]
        }
        None if dir.join("comparator.json").exists() => vec!["comparator"],
        None => vec!["check"],
    };
    let checker = format!("lake {}", args.join(" "));

    let runner = match runner {
        Runner::Auto if cfg!(target_os = "linux") && succeeds("bwrap", &["--version"]).await => Runner::Bwrap,
        Runner::Auto if succeeds("docker", &["info"]).await => Runner::Docker,
        Runner::Auto => bail!(
            "no sandbox available: install bubblewrap on Linux or start Docker. \
             Use `--runner none` to run without a sandbox (trusted projects only)."
        ),
        chosen => chosen,
    };

    let (name, output) = match runner {
        Runner::Bwrap => ("bwrap", lake(dir, &args).await?),
        Runner::None => (
            "none",
            lake(dir, &[&args[..], &["--inadvisably-no-sandbox"]].concat()).await?,
        ),
        Runner::Docker => ("docker+bwrap", docker(dir, &args, &toolchain, network).await?),
        Runner::Auto => unreachable!("resolved above"),
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let detail = tail(&text, 800);
    let result = match exit_code {
        0 => Verdict::Pass,
        1 if REJECTIONS.iter().any(|marker| text.contains(marker)) => Verdict::Fail,
        _ => {
            let refused = text.contains("Permission denied") || text.contains("Operation not permitted");
            let hint = if text.contains("bwrap:") && refused {
                format!("\n{USERNS_HINT}")
            } else {
                String::new()
            };
            bail!(
                "{checker} did not reach a verdict (exit code {exit_code}):\n{}{hint}",
                detail.unwrap_or_default()
            )
        }
    };
    Ok(Outcome {
        checker,
        toolchain,
        runner: name.into(),
        result,
        exit_code,
        detail,
    })
}

/// Returns true if `lake-manifest.json` declares external packages that require network resolution.
fn has_dependencies(dir: &Path) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(dir.join("lake-manifest.json")) else {
        // Missing manifests are reported directly by Lake with an actionable error.
        return Ok(false);
    };
    let manifest: serde_json::Value = serde_json::from_str(&text).context("lake-manifest.json is not valid JSON")?;
    Ok(manifest
        .get("packages")
        .and_then(|p| p.as_array())
        .is_some_and(|p| !p.is_empty()))
}

/// Reads and validates `lean-toolchain`. Restricts values to official Lean 4 releases before passing to elan.
fn toolchain(dir: &Path) -> Result<String> {
    let text = std::fs::read_to_string(dir.join("lean-toolchain"))
        .context("snapshot has no lean-toolchain file (not a Lean project)")?;
    let name = text.trim();
    let version = name.strip_prefix("leanprover/lean4:v").unwrap_or("");
    let plain = !version.is_empty()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !plain {
        bail!("unsupported lean-toolchain {name:?}; only official leanprover/lean4:v... releases are supported");
    }
    Ok(name.to_string())
}

async fn lake(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("lake")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run lake")
}

async fn docker(dir: &Path, args: &[&str], toolchain: &str, network: bool) -> Result<std::process::Output> {
    if !succeeds("docker", &["image", "inspect", IMAGE]).await {
        let mut build = Command::new("docker")
            .args(["build", "-t", IMAGE, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run docker")?;
        build
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(DOCKERFILE.as_bytes())
            .await?;
        let built = build.wait_with_output().await?;
        if !built.status.success() {
            bail!(
                "failed to build check Docker image:\n{}",
                String::from_utf8_lossy(&built.stderr)
            );
        }
    }

    let volume = format!("{TOOLCHAINS}:/home/checker/.elan");
    // Install the Lean toolchain first with networking enabled and before copying project files.
    let installed = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &volume,
            IMAGE,
            "elan",
            "toolchain",
            "install",
            toolchain,
        ])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !installed.status.success() {
        return Ok(installed);
    }

    // Copy files into the container rather than bind-mounting, since host mounts fail for paths
    // not shared with the Docker VM.
    let mut create = Command::new("docker");
    create.args(["create", "-v", &volume, "-w", "/work"]);
    // Disable Docker's seccomp, AppArmor, and masked /proc restrictions so bubblewrap can create
    // namespaces and mount a fresh /proc inside the container.
    for option in ["seccomp=unconfined", "apparmor=unconfined", "systempaths=unconfined"] {
        create.args(["--security-opt", option]);
    }
    if !network {
        create.args(["--network", "none"]);
    }
    create.args([IMAGE, "lake"]).args(args);
    let created = create.stdin(Stdio::null()).output().await?;
    if !created.status.success() {
        return Ok(created);
    }
    let container = String::from_utf8_lossy(&created.stdout).trim().to_string();

    let source = format!("{}/.", dir.display());
    let copied = Command::new("docker")
        .args(["cp", &source, &format!("{container}:/work")])
        .stdin(Stdio::null())
        .output()
        .await?;
    let output = if copied.status.success() {
        Command::new("docker")
            .args(["start", "--attach", &container])
            .stdin(Stdio::null())
            .output()
            .await?
    } else {
        copied
    };
    let _ = Command::new("docker")
        .args(["rm", "--force", &container])
        .stdin(Stdio::null())
        .output()
        .await;
    Ok(output)
}

async fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Returns the last `limit` characters of trimmed output, or `None` if empty.
fn tail(text: &str, limit: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let start = text.char_indices().rev().nth(limit - 1).map_or(0, |(at, _)| at);
    Some(text[start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_official_toolchains_accepted() {
        let dir = std::env::temp_dir().join(format!("delphi-toolchain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (text, ok) in [
            ("leanprover/lean4:v4.36.0\n", true),
            ("leanprover/lean4:v4.37.0-rc1", true),
            ("https://example.org/evil", false),
            ("leanprover/lean4:v4.36.0 && rm -rf /", false),
            ("", false),
        ] {
            std::fs::write(dir.join("lean-toolchain"), text).unwrap();
            assert_eq!(toolchain(&dir).is_ok(), ok, "{text:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_keeps_trailing_chars() {
        assert_eq!(tail("  \n", 5), None);
        assert_eq!(tail("abcdef", 3).as_deref(), Some("def"));
        assert_eq!(tail("ab", 3).as_deref(), Some("ab"));
    }
}
