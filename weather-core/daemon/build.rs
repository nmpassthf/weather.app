use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate should live under workspace/weather-core");
    let workspace_manifest = workspace_dir.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace_manifest.display());

    let workspace = read_toml(&workspace_manifest);
    let members = workspace
        .get("workspace")
        .and_then(|value| value.get("members"))
        .and_then(toml::Value::as_array)
        .expect("workspace members must be an array");
    let mut bin_names = BTreeSet::new();

    for member in members {
        let Some(member) = member.as_str() else {
            continue;
        };
        let member_dir = workspace_dir.join(member);
        let manifest = member_dir.join("Cargo.toml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        if !manifest.exists() {
            continue;
        }
        let value = read_toml(&manifest);
        if let Some(bins) = value.get("bin").and_then(toml::Value::as_array) {
            for bin in bins {
                if let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                    bin_names.insert(name.to_string());
                }
            }
        } else if member_dir.join("src/main.rs").exists()
            && let Some(name) = value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
        {
            bin_names.insert(name.to_string());
        }
    }

    println!(
        "cargo:rustc-env=WEATHER_WORKSPACE_BIN_NAMES={}",
        bin_names.into_iter().collect::<Vec<_>>().join(";")
    );
}

fn read_toml(path: &Path) -> toml::Value {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
        .parse()
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()))
}
