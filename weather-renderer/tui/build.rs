use std::process::Command;

fn main() {
    let build_version = git_short_hash().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_VERSION={build_version}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8(output.stdout).ok()?.trim().to_string();
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}
