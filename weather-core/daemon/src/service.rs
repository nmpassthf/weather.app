mod helper;
mod linux;
mod windows;

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::ServiceBackend;

/// 安装服务:user/system 模式,复制二进制,创建 bin/config 目录,写默认 config,生成 unit/服务定义。
pub(crate) fn install_service(
    backend: ServiceBackend,
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    match backend {
        ServiceBackend::Systemd => {
            linux::systemd::install(system, path_override, config_override, manage_service)
        }
        ServiceBackend::Windows => {
            windows::service_control::install(system, path_override, config_override)
        }
    }
}

/// 卸载服务:停服务 + 删 unit/服务定义 + 可选删 data/bin。
pub(crate) fn uninstall_service(
    backend: ServiceBackend,
    with_data: bool,
    with_bin: bool,
) -> Result<()> {
    match backend {
        ServiceBackend::Systemd => linux::systemd::uninstall(with_data, with_bin)?,
        ServiceBackend::Windows => windows::service_control::uninstall(with_data, with_bin)?,
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
    match backend {
        ServiceBackend::Systemd => {
            linux::systemd::reinstall(system, path_override, config_override, manage_service)
        }
        ServiceBackend::Windows => windows::service_control::reinstall(
            system,
            path_override,
            config_override,
            manage_service,
        ),
    }
}
