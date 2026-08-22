//! Workspace-wide checks in one command (blueprint §4, research #17):
//!
//! ```text
//! cargo xtask            # = cargo xtask check
//! cargo xtask check      # fmt + clippy + test + build across all members
//! ```
//!
//! Every cross-crate gate lives here so a local check is exactly the CI
//! check (Helix/cargo pattern).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "check".to_owned());
    let root = workspace_root();
    match task.as_str() {
        "check" => run_check(&root),
        other => {
            eprintln!("unknown xtask: {other}");
            eprintln!("usage: cargo xtask check");
            ExitCode::from(2)
        }
    }
}

/// The workspace root: this crate lives at `<root>/xtask`.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live one level below the workspace root")
        .to_owned()
}

fn run_check(root: &Path) -> ExitCode {
    let steps: &[(&str, &[&str])] = &[
        ("fmt", &["fmt", "--all", "--check"]),
        (
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("test", &["test", "--workspace"]),
        ("build", &["build", "--workspace"]),
    ];
    for (name, args) in steps {
        if !run_cargo(root, name, args) {
            eprintln!("xtask check failed at step '{name}'");
            return ExitCode::FAILURE;
        }
    }
    println!("xtask check: all green");
    ExitCode::SUCCESS
}

fn run_cargo(root: &Path, name: &str, args: &[&str]) -> bool {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    print!("[{name}] cargo {}", args.join(" "));
    let status = match Command::new(&cargo).args(args).current_dir(root).status() {
        Ok(status) => status,
        Err(err) => {
            eprintln!("\n[{name}] failed to spawn cargo: {err}");
            return false;
        }
    };
    if status.success() {
        println!(" — ok");
        true
    } else {
        eprintln!("\n[{name}] failed with {status}");
        false
    }
}
