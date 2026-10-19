//! Embeds the build identity into the binary.
//!
//! The firmware prints it on boot and reports it over Zigbee (Basic
//! `SWBuildID`/`DateCode`), so it is always clear which build is running.

use std::process::Command;

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    // `--tags` prefers the tag, `--always` still gives a commit when there is
    // no tag yet, `--dirty` marks a tree that was modified after the commit.
    let describe = run(&["git", "describe", "--tags", "--always", "--dirty", "--abbrev=7"])
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FANBRIDGE_GIT={describe}");

    let date = run(&["date", "-u", "+%Y%m%d"]).unwrap_or_else(|| "00000000".to_string());
    println!("cargo:rustc-env=FANBRIDGE_DATE={date}");
}
