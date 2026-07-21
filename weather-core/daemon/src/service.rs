mod helper;
mod linux;
#[cfg(windows)]
mod windows;

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
        ServiceBackend::Windows => {
            windows_install(system, path_override, config_override, manage_service)
        }
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
        ServiceBackend::Windows => windows_uninstall(
            system,
            path_override,
            config_override,
            with_data,
            with_bin,
            all,
        )?,
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
        ServiceBackend::Windows => {
            windows_reinstall(system, path_override, config_override, manage_service)
        }
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
        (ServiceBackend::Windows, ServicePlatform::Windows) => Ok(()),
        (ServiceBackend::Windows, _) => {
            bail!("Windows service backend is supported only on Windows")
        }
    }
}

pub(crate) fn run_windows_service(
    config: Option<PathBuf>,
    log_level: Option<crate::cli::DaemonLogLevel>,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows::run_dispatcher(config, log_level)
    }
    #[cfg(not(windows))]
    {
        let _ = (config, log_level);
        bail!("Windows service mode is supported only on Windows")
    }
}

fn windows_install(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows::install(system, path_override, config_override, manage_service)
    }
    #[cfg(not(windows))]
    {
        let _ = (system, path_override, config_override, manage_service);
        bail!("Windows service backend is supported only on Windows")
    }
}

fn windows_reinstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows::reinstall(system, path_override, config_override, manage_service)
    }
    #[cfg(not(windows))]
    {
        let _ = (system, path_override, config_override, manage_service);
        bail!("Windows service backend is supported only on Windows")
    }
}

fn windows_uninstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    with_data: bool,
    with_bin: bool,
    all: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows::uninstall(
            system,
            path_override,
            config_override,
            with_data,
            with_bin,
            all,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (
            system,
            path_override,
            config_override,
            with_data,
            with_bin,
            all,
        );
        bail!("Windows service backend is supported only on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_support_matrix_is_explicit() {
        assert!(
            validate_service_backend_for(ServiceBackend::Systemd, ServicePlatform::Linux).is_ok()
        );
        for platform in [ServicePlatform::Windows, ServicePlatform::Other] {
            let err = validate_service_backend_for(ServiceBackend::Systemd, platform).unwrap_err();
            assert!(err.to_string().contains("only on Linux"));
        }
        assert!(
            validate_service_backend_for(ServiceBackend::Windows, ServicePlatform::Windows).is_ok()
        );
        for platform in [ServicePlatform::Linux, ServicePlatform::Other] {
            let err = validate_service_backend_for(ServiceBackend::Windows, platform).unwrap_err();
            assert!(err.to_string().contains("only on Windows"));
        }
    }
}
