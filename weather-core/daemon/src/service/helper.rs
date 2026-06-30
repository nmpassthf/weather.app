use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use weather_configure::{ComponentKind, ComponentRegistry, default_config_toml};

pub(in crate::service) struct ServiceInstallFiles {
    pub(in crate::service) base: PathBuf,
    pub(in crate::service) bin_exe: PathBuf,
    pub(in crate::service) config_path: PathBuf,
}

pub(in crate::service) fn install_service_files(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
) -> Result<ServiceInstallFiles> {
    let base = resolve_base_path(system, path_override)?;
    let bin_dir = base.join("bin");
    let config_dir = base.join("config");
    fs::create_dir_all(&bin_dir).with_context(|| format!("mkdir {}", bin_dir.display()))?;
    fs::create_dir_all(&config_dir).with_context(|| format!("mkdir {}", config_dir.display()))?;
    let config_path = config_override.unwrap_or_else(|| config_dir.join("weather.toml"));
    let registry = ComponentRegistry::open(config_dir.join("component.list.db"))?;

    // 复制当前 daemon 二进制 + workspace 中其他同目录二进制(若存在)。
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let bin_exe = copy_binary(&exe, &bin_dir)?;
    registry.record(ComponentKind::Bin, &bin_exe)?;
    if let Some(parent) = exe.parent() {
        for sibling in sibling_binary_names() {
            let src = parent.join(&sibling);
            if src.exists() {
                let copied = copy_binary(&src, &bin_dir)?;
                registry.record(ComponentKind::Bin, copied)?;
            }
        }
    }

    // 默认 config: <base>/config/weather.toml,不存在则写默认模板。
    if !config_path.exists() {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, default_config_toml())
            .with_context(|| format!("write default config {}", config_path.display()))?;
    }
    registry.record(ComponentKind::Config, &config_path)?;

    Ok(ServiceInstallFiles {
        base,
        bin_exe,
        config_path,
    })
}

fn resolve_base_path(system: bool, path_override: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = path_override {
        return Ok(p);
    }
    if system {
        if cfg!(windows) {
            Ok(PathBuf::from(r"C:\Program Files\weather"))
        } else {
            Ok(PathBuf::from("/opt/weather"))
        }
    } else {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("neither HOME nor USERPROFILE is set")?;
        Ok(PathBuf::from(home).join(".weather"))
    }
}

fn copy_binary(src: &Path, bin_dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .with_context(|| format!("invalid exe path {}", src.display()))?;
    let dst = bin_dir.join(name);
    fs::copy(src, &dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&dst, perms)?;
    }
    println!("installed binary: {}", dst.display());
    Ok(dst)
}

pub(in crate::service) fn service_name() -> &'static str {
    option_env!("CARGO_BIN_NAME").unwrap_or(env!("CARGO_PKG_NAME"))
}

fn workspace_binary_names() -> Vec<&'static str> {
    env!("WEATHER_WORKSPACE_BIN_NAMES")
        .split(';')
        .filter(|name| !name.is_empty())
        .collect()
}

fn executable_name(bin_name: &str) -> String {
    if cfg!(windows) {
        format!("{bin_name}.exe")
    } else {
        bin_name.to_string()
    }
}

fn sibling_binary_names() -> Vec<String> {
    workspace_binary_names()
        .into_iter()
        .filter(|name| *name != service_name())
        .map(executable_name)
        .collect()
}

pub(in crate::service) fn cleanup_base(with_data: bool, with_bin: bool) -> Result<()> {
    // 默认 user base;system base 需 root,这里也尝试清理(用户需自行 sudo)。
    let bases = if std::env::var_os("HOME").is_some() {
        vec![
            PathBuf::from(std::env::var_os("HOME").unwrap()).join(".weather"),
            PathBuf::from("/opt/weather"),
        ]
    } else {
        vec![]
    };
    for base in bases {
        if !base.exists() {
            continue;
        }
        cleanup_registry(&base, with_data, with_bin)?;
    }
    Ok(())
}

fn cleanup_registry(base: &Path, with_data: bool, with_bin: bool) -> Result<()> {
    let registry_path = base.join("config").join("component.list.db");
    if !registry_path.exists() {
        return Ok(());
    }
    let registry = ComponentRegistry::open(&registry_path)?;
    for entry in registry.list()? {
        let should_remove = match entry.kind {
            ComponentKind::Config
            | ComponentKind::Db
            | ComponentKind::Lock
            | ComponentKind::Temp => with_data,
            ComponentKind::Bin => with_bin,
        };
        if should_remove {
            remove_component_path(&entry.path);
        }
    }
    if with_data {
        remove_component_path(&registry_path);
        remove_component_path(&sidecar_path(&registry_path, "db-wal"));
        remove_component_path(&sidecar_path(&registry_path, "db-shm"));
    }
    Ok(())
}

fn remove_component_path(path: &Path) {
    if path.is_dir() {
        if fs::remove_dir_all(path).is_ok() {
            println!("removed: {}", path.display());
        }
    } else if path.exists() && fs::remove_file(path).is_ok() {
        println!("removed: {}", path.display());
    }
}

fn sidecar_path(path: &Path, extension: &str) -> PathBuf {
    path.with_extension(extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn copy_binary_returns_destination_path() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "weather-copy-binary-test-{}-{}",
            std::process::id(),
            nanos
        ));
        let src_dir = base.join("src");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        let src = src_dir.join(executable_name(service_name()));
        fs::write(&src, b"binary").unwrap();

        let copied = copy_binary(&src, &bin_dir).unwrap();

        assert_eq!(copied, bin_dir.join(src.file_name().unwrap()));
        assert!(copied.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_binary_names_include_current_service_and_siblings() {
        let bins = workspace_binary_names();

        assert!(bins.iter().any(|name| *name == service_name()));
        assert!(bins.len() >= 2);
    }
}
