mod helper;
mod linux;

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::cli::ServiceBackend;

/// 安装服务:user/system 模式,复制二进制,创建 bin/config 目录,写默认 config,生成 unit/服务定义。
pub(crate) fn install_service(
    backend: ServiceBackend,
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    validate_service_backend(backend)?;
    match backend {
        ServiceBackend::Systemd => {
            linux::systemd::install(system, path_override, config_override, manage_service)
        }
        ServiceBackend::Windows => unsupported_windows_backend(),
    }
}

/// 卸载服务:停服务 + 删 unit/服务定义 + 可选删 data/bin。
pub(crate) fn uninstall_service(
    backend: ServiceBackend,
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    with_data: bool,
    with_bin: bool,
    all: bool,
) -> Result<()> {
    validate_service_backend(backend)?;
    match backend {
        ServiceBackend::Systemd => linux::systemd::uninstall(
            system,
            path_override,
            config_override,
            with_data || all,
            with_bin || all,
            all,
        )?,
        ServiceBackend::Windows => return unsupported_windows_backend(),
    }
    Ok(())
}

/// 重新安装已存在的服务。默认直接停止、重建并启动服务。
pub(crate) fn reinstall_service(
    backend: ServiceBackend,
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    validate_service_backend(backend)?;
    match backend {
        ServiceBackend::Systemd => {
            linux::systemd::reinstall(system, path_override, config_override, manage_service)
        }
        ServiceBackend::Windows => unsupported_windows_backend(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServicePlatform {
    Linux,
    Windows,
    Other,
}

fn current_service_platform() -> ServicePlatform {
    if cfg!(target_os = "linux") {
        ServicePlatform::Linux
    } else if cfg!(windows) {
        ServicePlatform::Windows
    } else {
        ServicePlatform::Other
    }
}

fn validate_service_backend(backend: ServiceBackend) -> Result<()> {
    validate_service_backend_for(backend, current_service_platform())
}

fn validate_service_backend_for(backend: ServiceBackend, platform: ServicePlatform) -> Result<()> {
    match (backend, platform) {
        (ServiceBackend::Systemd, ServicePlatform::Linux) => Ok(()),
        (ServiceBackend::Systemd, _) => {
            bail!("systemd service backend is supported only on Linux")
        }
        (ServiceBackend::Windows, _) => unsupported_windows_backend(),
    }
}

fn unsupported_windows_backend() -> Result<()> {
    bail!(
        "windows service backend is unsupported because weather-daemon does not implement an SCM service dispatcher"
    )
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn unique_test_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "weather-service-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn backend_support_matrix_is_explicit() {
        assert!(
            validate_service_backend_for(ServiceBackend::Systemd, ServicePlatform::Linux).is_ok()
        );
        for platform in [ServicePlatform::Windows, ServicePlatform::Other] {
            let err = validate_service_backend_for(ServiceBackend::Systemd, platform).unwrap_err();
            assert!(err.to_string().contains("only on Linux"));
        }
        for platform in [
            ServicePlatform::Linux,
            ServicePlatform::Windows,
            ServicePlatform::Other,
        ] {
            let err = validate_service_backend_for(ServiceBackend::Windows, platform).unwrap_err();
            assert!(err.to_string().contains("SCM service dispatcher"));
        }
    }

    #[test]
    fn windows_install_is_rejected_before_creating_files() {
        for manage_service in [true, false] {
            let base = unique_test_path(if manage_service {
                "windows-install"
            } else {
                "windows-manual-install"
            });
            let config = base.join("config/weather.toml");

            let err = install_service(
                ServiceBackend::Windows,
                false,
                Some(base.clone()),
                Some(config),
                manage_service,
            )
            .unwrap_err();

            assert!(err.to_string().contains("SCM service dispatcher"));
            assert!(!base.exists());
        }
    }

    #[test]
    fn windows_reinstall_is_rejected_before_creating_files() {
        for manage_service in [true, false] {
            let base = unique_test_path(if manage_service {
                "windows-reinstall"
            } else {
                "windows-manual-reinstall"
            });
            let config = base.join("config/weather.toml");

            let err = reinstall_service(
                ServiceBackend::Windows,
                false,
                Some(base.clone()),
                Some(config),
                manage_service,
            )
            .unwrap_err();

            assert!(err.to_string().contains("SCM service dispatcher"));
            assert!(!base.exists());
        }
    }

    #[test]
    fn windows_remove_is_explicitly_rejected() {
        let err = uninstall_service(
            ServiceBackend::Windows,
            false,
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("SCM service dispatcher"));
    }
}
