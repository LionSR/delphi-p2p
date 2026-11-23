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

const IMAGE: &str = "delphi-lean-check:4";
/// Docker volume name for caching downloaded Lean toolchains across runs.
const TOOLCHAINS: &str = "delphi-elan";
const DOCKERFILE: &str = include_str!("../docker/lean-check.Dockerfile");

/// Only a manifest that pins Mathlib from here gets Mathlib's prebuilt files.
const MATHLIB: &str = "https://github.com/leanprover-community/mathlib4";

/// Fetches Mathlib's prebuilt files into `/work/.lake/packages`. Arguments: the Mathlib revision
/// and the toolchain.
///
/// It runs in a container of its own, which holds no file of the project under check: a
/// throwaway project requires Mathlib at the pinned revision and nothing else, and Mathlib's own
/// tool downloads the files. The revision comes from the project's manifest, so what runs here is
/// not trusted either. It can write to the packages volume and nothing else that outlasts the
/// container; Lake's sandbox and the kernel replay then treat the packages as they treat any
/// dependency.
const FETCH_MATHLIB: &str = r#"set -e
printf '%s\n' "$2" > lean-toolchain
printf 'name = "mathlibcache"\n\n[[require]]\nname = "mathlib"\ngit = "%s"\nrev = "%s"\n' "$MATHLIB" "$1" > lakefile.toml
lake update
lake exe cache get
"#;

/// Runs the Lean checker on the project in `dir`.
///
/// `comparator` is an optional relative path to a comparator config inside `dir`.
/// Setting `network` enables network access in the container so Lake can fetch package
/// dependencies (such as Mathlib), which also grants project build code network access.
/// With `mathlib_cache`, a project that pins Mathlib gets Mathlib's prebuilt files instead of
/// compiling Mathlib, when the network is allowed and the runner is Docker.
pub async fn run(
    dir: &Path,
    comparator: Option<&str>,
    runner: Runner,
    network: bool,
    mathlib_cache: bool,
) -> Result<Outcome> {
    let toolchain = toolchain(dir)?;
    if !network && has_dependencies(dir)? {
        bail!("project has external dependencies that require network access; re-run with --network");
    }
    let pinned = if mathlib_cache && network {
        mathlib_rev(dir)?
    } else {
        None
    };
    let args: Vec<&str> = match comparator {
        Some(config) => {
            // Validate remote claim path before passing it as a CLI argument.
            files::checked(config)?;
            vec!["comparator", "--config", config]
        }
        None if dir.join("comparator.json").exists() => vec!["comparator"],
        None => vec!["check"],
    };

    let runner = match runner {
        Runner::Auto if cfg!(target_os = "linux") && succeeds("bwrap", &["--version"]).await => Runner::Bwrap,
        Runner::Auto if succeeds("docker", &["info"]).await => Runner::Docker,
        Runner::Auto => bail!(
            "no sandbox available: install bubblewrap on Linux or start Docker. \
             Use `--runner none` to run without a sandbox (trusted projects only)."
        ),
        chosen => chosen,
    };
    let mathlib = match pinned {
        Some(rev) if runner == Runner::Docker => Some(rev),
        Some(_) => {
            eprintln!(
                "delphi-p2p: Mathlib will be compiled from source. Its prebuilt files are fetched with the Docker \
                 runner only (--runner docker)."
            );
            None
        }
        None => None,
    };
    // The claim says where Mathlib's compiled files came from.
    let checker = match mathlib {
        Some(_) => format!("lake {} (Mathlib from its cache)", args.join(" ")),
        None => format!("lake {}", args.join(" ")),
    };

    let (name, output) = match runner {
        Runner::Bwrap => ("bwrap", lake(dir, &args).await?),
        Runner::None => (
            "none",
            lake(dir, &[&args[..], &["--inadvisably-no-sandbox"]].concat()).await?,
        ),
        Runner::Docker => (
            "docker+bwrap",
            docker(dir, &args, &toolchain, network, mathlib.as_deref()).await?,
        ),
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

/// The revision of Mathlib that `lake-manifest.json` pins, if it takes Mathlib from its own
/// repository. A fork, or a revision that is not a full commit hash, gives `None`.
fn mathlib_rev(dir: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(dir.join("lake-manifest.json")).context("snapshot has no lake-manifest.json")?;
    let manifest: serde_json::Value = serde_json::from_str(&text).context("lake-manifest.json is not valid JSON")?;
    let packages = manifest.get("packages").and_then(|p| p.as_array());
    Ok(packages.into_iter().flatten().find_map(|package| {
        let field = |name| package.get(name).and_then(|v| v.as_str());
        let url = field("url")?.trim_end_matches('/').trim_end_matches(".git");
        let rev = field("rev")?;
        let pinned = rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit());
        (field("name") == Some("mathlib") && url == MATHLIB && pinned).then(|| rev.to_string())
    }))
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

async fn docker(
    dir: &Path,
    args: &[&str],
    toolchain: &str,
    network: bool,
    mathlib: Option<&str>,
) -> Result<std::process::Output> {
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

    let toolchains = format!("{TOOLCHAINS}:/home/checker/.elan");
    // Install the Lean toolchain first with networking enabled and before copying project files.
    let installed = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &toolchains,
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

    // Mathlib's prebuilt files go into a volume of this one check, which becomes the project's
    // `.lake/packages`. A volume kept across checks could be changed by one project for the next.
    let packages = mathlib.map(|_| format!("delphi-mathlib-{}", std::process::id()));
    let output = match (mathlib, &packages) {
        (Some(rev), Some(volume)) => match fetch_mathlib(rev, toolchain, volume).await {
            Ok(()) => check_in_container(dir, args, &toolchains, network, Some(volume)).await,
            Err(err) => Err(err),
        },
        _ => check_in_container(dir, args, &toolchains, network, None).await,
    };
    if let Some(volume) = &packages {
        let _ = Command::new("docker")
            .args(["volume", "rm", "--force", volume])
            .stdin(Stdio::null())
            .output()
            .await;
    }
    output
}

/// Runs [`FETCH_MATHLIB`] in a container that sees the toolchains read-only and the packages
/// volume, with no file of the project under check. A failure is never a verdict.
async fn fetch_mathlib(rev: &str, toolchain: &str, volume: &str) -> Result<()> {
    let fetched = Command::new("docker")
        .args(["run", "--rm", "-w", "/work"])
        .args(["-v", &format!("{TOOLCHAINS}:/home/checker/.elan:ro")])
        .args(["-v", &format!("{volume}:/work/.lake/packages")])
        .args(["--env", &format!("MATHLIB={MATHLIB}"), IMAGE])
        .args(["sh", "-c", FETCH_MATHLIB, "sh", rev, toolchain])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !fetched.status.success() {
        let mut text = String::from_utf8_lossy(&fetched.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&fetched.stderr));
        bail!(
            "could not fetch Mathlib's prebuilt files; `--no-mathlib-cache` compiles Mathlib instead:\n{}",
            tail(&text, 800).unwrap_or_default()
        );
    }
    Ok(())
}

/// Copies the project into a fresh container and runs Lake's checker there.
async fn check_in_container(
    dir: &Path,
    args: &[&str],
    toolchains: &str,
    network: bool,
    packages: Option<&str>,
) -> Result<std::process::Output> {
    // Copy files into the container rather than bind-mounting, since host mounts fail for paths
    // not shared with the Docker VM.
    let mut create = Command::new("docker");
    create.args(["create", "-v", toolchains, "-w", "/work"]);
    if let Some(volume) = packages {
        create.args(["-v", &format!("{volume}:/work/.lake/packages")]);
    }
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
    fn mathlib_cache_only_for_mathlib_from_its_own_repository() {
        let dir = std::env::temp_dir().join(format!("delphi-mathlib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rev = "065356127b1dc0016f66b7283ce0ce2c4055aa55";
        let manifest = |name: &str, url: &str, rev: &str| {
            format!(r#"{{"packages": [{{"name": "{name}", "url": "{url}", "rev": "{rev}"}}]}}"#)
        };
        for (text, pinned) in [
            (manifest("mathlib", MATHLIB, rev), true),
            (manifest("mathlib", &format!("{MATHLIB}.git"), rev), true),
            (manifest("mathlib", "https://github.com/someone/mathlib4", rev), false),
            (manifest("batteries", MATHLIB, rev), false),
            (manifest("mathlib", MATHLIB, "master"), false),
            (r#"{"packages": []}"#.to_string(), false),
        ] {
            std::fs::write(dir.join("lake-manifest.json"), &text).unwrap();
            assert_eq!(mathlib_rev(&dir).unwrap().is_some(), pinned, "{text}");
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
