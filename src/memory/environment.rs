//! The `wake` payload's environment notes (v0.3 roadmap #1 / B2) — the
//! guidance surface ADR-0017 promised: cheap, synchronous probes of the
//! semantic tooling around the repo, so the standing guidance can say
//! "rust-analyzer is available, cross-check against it" with evidence
//! instead of referencing a field that does not exist.
//!
//! Probes are deliberately dumb:
//!
//! - **Language servers** — a pure PATH lookup per known server name (an
//!   executable file in some PATH directory). No process is spawned: the
//!   question is "could the agent run it", not "what version is it".
//! - **git** — present via the same PATH lookup; `dirty` runs ONE
//!   `git status --porcelain` in the repo directory (the only spawn, the
//!   same subprocess posture as `--commit` validation, ADR-0013). Git
//!   missing or the directory not a repository ⇒ `dirty: false`
//!   (nothing is uncommitted).
//!
//! Still guidance, not a runtime (ADR-0017 unchanged): janitor never
//! starts, embeds, or consults a language server — it only reports what
//! the PATH offers, and the probes run on every `wake`.

use std::ffi::OsStr;
use std::path::Path;

use serde::Serialize;

/// The language servers the probe looks for — one entry per executable
/// name, in this (fixed, report-stable) order. `pyright` and
/// `basedpyright` are distinct binaries for the same role and stay
/// separate rows (the consumer decides equivalence).
pub const PROBED_SERVERS: &[&str] = &[
    "rust-analyzer",
    "pyright",
    "basedpyright",
    "gopls",
    "ruby-lsp",
    "elixir-ls",
    "ocamllsp",
    "typescript-language-server",
];

/// One probed language server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerProbe {
    /// The executable name looked up on the PATH.
    pub name: &'static str,
    /// An executable file with this name exists in some PATH directory.
    pub available: bool,
}

/// The git probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct GitProbe {
    /// `git` is on the PATH.
    pub available: bool,
    /// `git status --porcelain` listed anything in the repo directory
    /// (staged, unstaged, or untracked). False when git is missing or the
    /// directory is not a repository — nothing is uncommitted either way.
    pub dirty: bool,
}

/// The `wake` payload's `environment` block (v0.3): what the semantic
/// layer around this repo looks like, per the probes above.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EnvironmentNotes {
    /// This binary's own version (`--version`'s `data.version`).
    pub janitor: &'static str,
    /// One row per [`PROBED_SERVERS`] entry, in that order.
    pub servers: Vec<ServerProbe>,
    pub git: GitProbe,
}

/// Whether `name` resolves to an executable file in one of the PATH
/// directories — the std-only `which`. Pure over its arguments, so tests
/// never mutate the process environment.
pub fn on_path(path_value: Option<&OsStr>, name: &str) -> bool {
    path_value.is_some_and(|value| {
        std::env::split_paths(value).any(|dir| is_executable_file(&dir.join(name)))
    })
}

/// A regular file with any execute bit set (unix; the crate's platform).
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Probe every known server against one PATH value (pure — the test seam
/// behind [`probe`]).
pub fn probe_servers(path_value: Option<&OsStr>) -> Vec<ServerProbe> {
    PROBED_SERVERS
        .iter()
        .map(|&name| ServerProbe {
            name,
            available: on_path(path_value, name),
        })
        .collect()
}

/// Probe git: PATH presence, then one `git status --porcelain` run in
/// `dir` for the dirty flag (pure over its arguments).
pub fn probe_git(path_value: Option<&OsStr>, dir: &Path) -> GitProbe {
    if !on_path(path_value, "git") {
        return GitProbe {
            available: false,
            dirty: false,
        };
    }
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false);
    GitProbe {
        available: true,
        dirty,
    }
}

/// The environment block for a wake payload rooted at `repo` (the
/// `.janitor/` parent): every probe against the process PATH.
pub fn probe(repo: &Path) -> EnvironmentNotes {
    let path = std::env::var_os("PATH");
    EnvironmentNotes {
        janitor: env!("CARGO_PKG_VERSION"),
        servers: probe_servers(path.as_deref()),
        git: probe_git(path.as_deref(), repo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An executable `name` under a scratch bin dir, mode 755.
    fn scratch_bin(name: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join(name);
        std::fs::write(&file, b"#!/bin/sh\nexit 0\n").expect("write server");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
        tmp
    }

    /// The PATH lookup is pure over its argument: a PATH containing
    /// rust-analyzer detects rust-analyzer (and nothing else), and a PATH
    /// without it does not — the acceptance probe for roadmap #1.
    #[test]
    fn a_path_containing_rust_analyzer_detects_rust_analyzer() {
        let bin = scratch_bin("rust-analyzer");
        let path = std::ffi::OsString::from(bin.path());

        let probes = probe_servers(Some(path.as_os_str()));
        assert_eq!(probes.len(), PROBED_SERVERS.len());
        assert_eq!(probes[0].name, "rust-analyzer");
        assert!(probes[0].available, "rust-analyzer is on this PATH");
        for probe in &probes[1..] {
            assert!(!probe.available, "{} must not resolve", probe.name);
        }

        // Control: a PATH without it (and no PATH at all) finds nothing.
        let empty = tempfile::tempdir().expect("tempdir");
        let empty_path = std::ffi::OsString::from(empty.path());
        assert!(
            probe_servers(Some(empty_path.as_os_str()))
                .iter()
                .all(|probe| !probe.available)
        );
        assert!(probe_servers(None).iter().all(|probe| !probe.available));
    }

    /// A non-executable file with the right name is not a server (the
    /// probe answers "could the agent run it", not "does the name exist").
    #[test]
    fn a_non_executable_name_is_not_available() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("gopls"), b"not executable").expect("write gopls");
        let path = std::ffi::OsString::from(tmp.path());
        let probes = probe_servers(Some(path.as_os_str()));
        assert!(
            probes
                .iter()
                .find(|probe| probe.name == "gopls")
                .is_some_and(|probe| !probe.available)
        );
    }

    /// The git probe: absent from the PATH ⇒ unavailable, not-dirty; on a
    /// real repository, clean after the commit and dirty after an edit
    /// (untracked files count — porcelain lists them).
    #[test]
    fn git_probe_reports_presence_and_dirt() {
        let nowhere = tempfile::tempdir().expect("tempdir");
        let empty_path = std::ffi::OsString::from(nowhere.path());
        assert_eq!(
            probe_git(Some(empty_path.as_os_str()), nowhere.path()),
            GitProbe {
                available: false,
                dirty: false
            }
        );

        // A real fixture (the memory e2e lane's subprocess posture).
        let repo = tempfile::tempdir().expect("repo");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.name", "probe"]);
        git(&["config", "user.email", "probe@example.invalid"]);
        std::fs::write(repo.path().join("lib.rs"), "fn a() {}\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "fixture"]);

        let real_path = std::env::var_os("PATH").expect("CI/dev machines have git on PATH");
        assert_eq!(
            probe_git(Some(real_path.as_os_str()), repo.path()),
            GitProbe {
                available: true,
                dirty: false
            },
            "the committed tree is clean"
        );
        std::fs::write(repo.path().join("dirty.rs"), "fn b() {}\n").expect("untracked edit");
        assert_eq!(
            probe_git(Some(real_path.as_os_str()), repo.path()),
            GitProbe {
                available: true,
                dirty: true
            },
            "an untracked file is an uncommitted change"
        );

        // Not a repository: git present, dirty still false.
        let plain = tempfile::tempdir().expect("plain dir");
        assert_eq!(
            probe_git(Some(real_path.as_os_str()), plain.path()),
            GitProbe {
                available: true,
                dirty: false
            }
        );
    }

    /// The block's serialized shape is the documented contract: janitor
    /// version, one {name, available} row per probed server in fixed
    /// order, and the git {available, dirty} pair — exact key sets.
    #[test]
    fn environment_notes_serialize_to_the_documented_shape() {
        let keys_of = |value: &serde_json::Value| -> Vec<String> {
            value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        let notes = EnvironmentNotes {
            janitor: env!("CARGO_PKG_VERSION"),
            servers: probe_servers(None),
            git: GitProbe {
                available: false,
                dirty: false,
            },
        };
        let json = serde_json::to_value(&notes).expect("json");
        assert_eq!(json["janitor"], env!("CARGO_PKG_VERSION"));
        let servers = json["servers"].as_array().expect("servers is an array");
        assert_eq!(servers.len(), PROBED_SERVERS.len());
        for (row, name) in servers.iter().zip(PROBED_SERVERS) {
            assert_eq!(
                keys_of(row),
                ["available", "name"].map(String::from),
                "exact key set"
            );
            assert_eq!(row["name"], *name);
            assert_eq!(row["available"], false);
        }
        assert_eq!(
            keys_of(&json["git"]),
            ["available", "dirty"].map(String::from)
        );
    }
}
