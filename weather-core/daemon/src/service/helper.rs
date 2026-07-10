use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use weather_configure::{ComponentKind, ComponentRegistry, default_config_toml};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::service) struct ServiceLayout {
    pub(in crate::service) system: bool,
    pub(in crate::service) base: PathBuf,
    pub(in crate::service) bin_dir: PathBuf,
    pub(in crate::service) config_path: PathBuf,
    pub(in crate::service) registry_path: PathBuf,
}

impl ServiceLayout {
    pub(in crate::service) fn resolve(
        system: bool,
        path_override: Option<PathBuf>,
        config_override: Option<PathBuf>,
    ) -> Result<Self> {
        let base = absolute_path(resolve_base_path(system, path_override)?)?;
        let bin_dir = base.join("bin");
        let config_path = match config_override {
            Some(path) => absolute_path(path)?,
            None => base.join("config/weather.toml"),
        };
        let config_dir = config_path.parent().with_context(|| {
            format!(
                "service config path has no parent: {}",
                config_path.display()
            )
        })?;
        let registry_path = config_dir.join("component.list.db");
        Ok(Self {
            system,
            base,
            bin_dir,
            config_path,
            registry_path,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::service) struct ServiceCleanupOptions {
    pub(in crate::service) with_data: bool,
    pub(in crate::service) with_bin: bool,
    pub(in crate::service) remove_registry: bool,
}

pub(in crate::service) struct ServiceInstallFiles {
    pub(in crate::service) bin_exe: PathBuf,
}

pub(in crate::service) fn install_service_files(
    layout: &ServiceLayout,
) -> Result<ServiceInstallFiles> {
    fs::create_dir_all(&layout.bin_dir)
        .with_context(|| format!("mkdir {}", layout.bin_dir.display()))?;
    let config_dir = layout.config_path.parent().with_context(|| {
        format!(
            "service config path has no parent: {}",
            layout.config_path.display()
        )
    })?;
    fs::create_dir_all(config_dir).with_context(|| format!("mkdir {}", config_dir.display()))?;
    let registry = ComponentRegistry::open(&layout.registry_path)?;

    // 复制当前 daemon 二进制 + workspace 中其他同目录二进制(若存在)。
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let bin_exe = copy_binary(&exe, &layout.bin_dir)?;
    registry.record(ComponentKind::Bin, &bin_exe)?;
    if let Some(parent) = exe.parent() {
        for sibling in sibling_binary_names() {
            let src = parent.join(&sibling);
            if src
                .try_exists()
                .with_context(|| format!("inspect sibling binary {}", src.display()))?
            {
                let copied = copy_binary(&src, &layout.bin_dir)?;
                registry.record(ComponentKind::Bin, copied)?;
            }
        }
    }

    // 默认 config: <base>/config/weather.toml,不存在则写默认模板。
    if !layout
        .config_path
        .try_exists()
        .with_context(|| format!("inspect config {}", layout.config_path.display()))?
    {
        fs::write(&layout.config_path, default_config_toml())
            .with_context(|| format!("write default config {}", layout.config_path.display()))?;
    }
    registry.record(ComponentKind::Config, &layout.config_path)?;

    Ok(ServiceInstallFiles { bin_exe })
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

fn resolve_base_path(system: bool, path_override: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = path_override {
        return Ok(p);
    }
    if system {
        Ok(PathBuf::from("/opt/weather"))
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
    if paths_refer_to_same_file(src, &dst)? {
        println!("binary already installed: {}", dst.display());
        return Ok(dst);
    }
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

#[cfg(unix)]
fn paths_refer_to_same_file(src: &Path, dst: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let src_metadata =
        fs::metadata(src).with_context(|| format!("inspect source binary {}", src.display()))?;
    let dst_metadata = match fs::metadata(dst) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("inspect installed binary {}", dst.display()));
        }
    };

    Ok(src_metadata.dev() == dst_metadata.dev() && src_metadata.ino() == dst_metadata.ino())
}

#[cfg(not(unix))]
fn paths_refer_to_same_file(src: &Path, dst: &Path) -> Result<bool> {
    if !dst
        .try_exists()
        .with_context(|| format!("inspect installed binary {}", dst.display()))?
    {
        return Ok(false);
    }
    let canonical_src = fs::canonicalize(src)
        .with_context(|| format!("canonicalize source binary {}", src.display()))?;
    let canonical_dst = fs::canonicalize(dst)
        .with_context(|| format!("canonicalize installed binary {}", dst.display()))?;
    Ok(canonical_src == canonical_dst)
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

pub(in crate::service) fn cleanup_service_layout(
    layout: &ServiceLayout,
    options: ServiceCleanupOptions,
) -> Result<()> {
    if !options.with_data && !options.with_bin && !options.remove_registry {
        return Ok(());
    }
    let registry_exists = layout
        .registry_path
        .try_exists()
        .with_context(|| format!("inspect registry {}", layout.registry_path.display()))?;
    let entries = if registry_exists {
        ComponentRegistry::open(&layout.registry_path)?.list()?
    } else {
        Vec::new()
    };

    for entry in entries {
        let should_remove = match entry.kind {
            ComponentKind::Config
            | ComponentKind::Db
            | ComponentKind::Lock
            | ComponentKind::Temp => options.with_data,
            ComponentKind::Bin => options.with_bin,
        };
        if should_remove {
            remove_component_path(&entry.path)?;
        }
    }

    if options.with_data {
        remove_component_path(&layout.config_path)?;
    }
    if options.with_bin {
        remove_component_path(&layout.bin_dir)?;
    }
    if options.remove_registry {
        remove_component_path(&layout.registry_path)?;
        remove_component_path(&sidecar_path(&layout.registry_path, "db-wal"))?;
        remove_component_path(&sidecar_path(&layout.registry_path, "db-shm"))?;
    }
    Ok(())
}

pub(in crate::service) fn remove_component_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("inspect component {}", path.display()));
        }
    };
    let result = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => {
            println!("removed: {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove component {}", path.display())),
    }
}

fn sidecar_path(path: &Path, extension: &str) -> PathBuf {
    path.with_extension(extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "weather-service-helper-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn copy_binary_returns_destination_path() {
        let base = unique_test_dir("copy");
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
    fn service_layout_uses_the_external_config_registry() {
        let root = unique_test_dir("external-layout");
        let base = root.join("base");
        let config = root.join("external/weather.toml");

        let layout =
            ServiceLayout::resolve(true, Some(base.clone()), Some(config.clone())).unwrap();

        assert!(layout.system);
        assert_eq!(layout.base, base);
        assert_eq!(layout.bin_dir, root.join("base/bin"));
        assert_eq!(layout.config_path, config);
        assert_eq!(
            layout.registry_path,
            root.join("external/component.list.db")
        );
    }

    #[test]
    fn install_records_components_next_to_an_external_config() {
        let root = unique_test_dir("external-install");
        let layout = ServiceLayout::resolve(
            false,
            Some(root.join("base")),
            Some(root.join("external/weather.toml")),
        )
        .unwrap();

        let files = install_service_files(&layout).unwrap();

        assert!(files.bin_exe.is_file());
        assert!(layout.config_path.is_file());
        assert!(layout.registry_path.is_file());
        assert!(!root.join("base/config/component.list.db").exists());
        let entries = ComponentRegistry::open(&layout.registry_path)
            .unwrap()
            .list()
            .unwrap();
        assert!(entries.iter().any(|entry| {
            entry.kind == ComponentKind::Config && entry.path == layout.config_path
        }));
        assert!(
            entries
                .iter()
                .any(|entry| { entry.kind == ComponentKind::Bin && entry.path == files.bin_exe })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_binary_skips_the_same_file() {
        let base = unique_test_dir("same-file");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let src = bin_dir.join(executable_name(service_name()));
        fs::write(&src, b"unchanged").unwrap();

        let copied = copy_binary(&src, &bin_dir).unwrap();

        assert_eq!(copied, src);
        assert_eq!(fs::read(&copied).unwrap(), b"unchanged");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_binary_skips_a_hard_link_to_the_destination() {
        let base = unique_test_dir("hard-link");
        let src_dir = base.join("src");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        let name = executable_name(service_name());
        let src = src_dir.join(&name);
        let dst = bin_dir.join(&name);
        fs::write(&src, b"hard-linked").unwrap();
        fs::hard_link(&src, &dst).unwrap();

        let copied = copy_binary(&src, &bin_dir).unwrap();

        assert_eq!(copied, dst);
        assert_eq!(fs::read(&copied).unwrap(), b"hard-linked");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn copy_binary_skips_a_symlink_to_the_destination() {
        use std::os::unix::fs::symlink;

        let base = unique_test_dir("symbolic-link");
        let src_dir = base.join("src");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        let name = executable_name(service_name());
        let src = src_dir.join(&name);
        let dst = bin_dir.join(&name);
        fs::write(&dst, b"symlinked").unwrap();
        symlink(&dst, &src).unwrap();

        let copied = copy_binary(&src, &bin_dir).unwrap();

        assert_eq!(copied, dst);
        assert_eq!(fs::read(&copied).unwrap(), b"symlinked");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_removes_custom_external_config_components() {
        let root = unique_test_dir("external-cleanup");
        let layout = populated_layout(&root);
        let db = root.join("external/weather.db");
        let lock = root.join("external/weather.db.lock");
        let temporary = root.join("external/tmp");

        cleanup_service_layout(
            &layout,
            ServiceCleanupOptions {
                with_data: true,
                with_bin: true,
                remove_registry: true,
            },
        )
        .unwrap();

        assert!(!layout.config_path.exists());
        assert!(!layout.bin_dir.exists());
        assert!(!layout.registry_path.exists());
        assert!(!db.exists());
        assert!(!lock.exists());
        assert!(!temporary.exists());
        assert!(root.join("keep.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_cleanup_is_repeatable_and_preserves_the_registry() {
        let root = unique_test_dir("partial-cleanup");
        let layout = populated_layout(&root);
        let data_only = ServiceCleanupOptions {
            with_data: true,
            with_bin: false,
            remove_registry: false,
        };
        let bin_only = ServiceCleanupOptions {
            with_data: false,
            with_bin: true,
            remove_registry: false,
        };

        cleanup_service_layout(&layout, data_only).unwrap();
        cleanup_service_layout(&layout, data_only).unwrap();
        assert!(!layout.config_path.exists());
        assert!(layout.bin_dir.exists());
        assert!(layout.registry_path.exists());

        cleanup_service_layout(&layout, bin_only).unwrap();
        cleanup_service_layout(&layout, bin_only).unwrap();
        assert!(!layout.bin_dir.exists());
        assert!(layout.registry_path.exists());

        cleanup_service_layout(
            &layout,
            ServiceCleanupOptions {
                with_data: true,
                with_bin: true,
                remove_registry: true,
            },
        )
        .unwrap();
        assert!(!layout.registry_path.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_ignores_only_missing_paths() {
        let root = unique_test_dir("cleanup-errors");
        fs::create_dir_all(&root).unwrap();
        remove_component_path(&root.join("missing")).unwrap();
        let blocking_file = root.join("file");
        fs::write(&blocking_file, b"file").unwrap();

        let err = remove_component_path(&blocking_file.join("child")).unwrap_err();

        assert!(err.to_string().contains("inspect component"));
        let _ = fs::remove_dir_all(&root);
    }

    fn populated_layout(root: &Path) -> ServiceLayout {
        let layout = ServiceLayout::resolve(
            false,
            Some(root.join("base")),
            Some(root.join("external/weather.toml")),
        )
        .unwrap();
        fs::create_dir_all(&layout.bin_dir).unwrap();
        fs::create_dir_all(layout.config_path.parent().unwrap()).unwrap();
        fs::write(&layout.config_path, b"config").unwrap();
        let bin = layout.bin_dir.join(executable_name(service_name()));
        let db = root.join("external/weather.db");
        let lock = root.join("external/weather.db.lock");
        let temporary = root.join("external/tmp");
        fs::write(&bin, b"bin").unwrap();
        fs::write(&db, b"db").unwrap();
        fs::write(&lock, b"lock").unwrap();
        fs::create_dir_all(&temporary).unwrap();
        fs::write(temporary.join("part"), b"temp").unwrap();
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        let registry = ComponentRegistry::open(&layout.registry_path).unwrap();
        registry
            .record(ComponentKind::Config, &layout.config_path)
            .unwrap();
        registry.record(ComponentKind::Bin, &bin).unwrap();
        registry.record(ComponentKind::Db, &db).unwrap();
        registry.record(ComponentKind::Lock, &lock).unwrap();
        registry.record(ComponentKind::Temp, &temporary).unwrap();
        layout
    }

    #[test]
    fn workspace_binary_names_include_current_service_and_siblings() {
        let bins = workspace_binary_names();

        assert!(bins.iter().any(|name| *name == service_name()));
        assert!(bins.len() >= 2);
    }
}
