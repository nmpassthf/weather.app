use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// Emit a reproducible build version and every tracked input needed to refresh it.
pub fn emit_git_version(variable: &str) {
    const OVERRIDE: &str = "WEATHER_BUILD_VERSION";

    println!("cargo:rerun-if-env-changed={OVERRIDE}");
    if let Some(version) = env::var(OVERRIDE)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        emit_version(variable, &version);
        return;
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
    let version = workspace_root(&manifest_dir)
        .inspect(|root| emit_git_inputs(root))
        .and_then(|root| git_version(&root))
        .unwrap_or(package_version);
    emit_version(variable, &version);
}

fn emit_version(variable: &str, version: &str) {
    println!("cargo:rustc-env={variable}={version}");
}

fn workspace_root(directory: &Path) -> Option<PathBuf> {
    git_text(directory, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn emit_git_inputs(root: &Path) {
    for spec in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path(root, spec)
            && path.exists()
        {
            emit_path(&path);
        }
    }
    if let Some(reference) = git_text(root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_path(root, &reference)
        && path.exists()
    {
        emit_path(&path);
    }

    let Some(output) = git_output(root, &["ls-files", "-z"]) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for path in output.stdout.split(|byte| *byte == 0) {
        if path.is_empty() {
            continue;
        }
        if let Ok(path) = std::str::from_utf8(path) {
            emit_path(&root.join(path));
        }
    }
}

fn emit_path(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

fn git_path(root: &Path, spec: &str) -> Option<PathBuf> {
    let path = git_text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", spec],
    )
    .or_else(|| git_text(root, &["rev-parse", "--git-path", spec]))?;
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn git_version(root: &Path) -> Option<String> {
    let hash = git_text(root, &["rev-parse", "--short=8", "HEAD"])?;
    let status = git_output(root, &["status", "--porcelain=v1", "--untracked-files=no"])?;
    if !status.status.success() {
        return None;
    }
    Some(decorate_version(&hash, !status.stdout.is_empty()))
}

fn decorate_version(hash: &str, dirty: bool) -> String {
    if dirty {
        format!("{hash}-dirty")
    } else {
        hash.to_string()
    }
}

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn git_output(root: &Path, args: &[&str]) -> Option<Output> {
    Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_suffix_is_stable() {
        assert_eq!(decorate_version("0123abcd", false), "0123abcd");
        assert_eq!(decorate_version("0123abcd", true), "0123abcd-dirty");
    }
}
