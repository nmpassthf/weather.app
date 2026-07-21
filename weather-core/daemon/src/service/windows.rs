use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::oneshot;
use windows_service::{
    define_windows_service,
    service::{
        Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
        ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_sys::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
};

use crate::{
    cli::DaemonLogLevel,
    run::run,
    service::helper::{
        ServiceCleanupOptions, ServiceLayout, cleanup_service_layout, install_service_files,
        service_name,
    },
    stop::stop,
};

const SERVICE_DISPLAY_NAME: &str = "Weather Engine";
const SERVICE_DESCRIPTION: &str = "Local weather data engine for Weather App";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const SERVICE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct DispatcherOptions {
    config: Option<PathBuf>,
    log_level: Option<DaemonLogLevel>,
}

static DISPATCHER_OPTIONS: OnceLock<DispatcherOptions> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

pub(crate) fn install(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    require_system_scope(system)?;
    let layout = ServiceLayout::resolve(true, path_override, config_override)?;
    let files = install_service_files(&layout)?;
    if manage_service {
        create_and_start_service(&layout, &files.bin_exe)?;
    } else {
        print_manual_install(&layout, &files.bin_exe);
    }
    Ok(())
}

pub(crate) fn reinstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    require_system_scope(system)?;
    let layout = ServiceLayout::resolve(true, path_override, config_override)?;
    if manage_service {
        remove_service_definition()?;
    }
    let files = install_service_files(&layout)?;
    if manage_service {
        create_and_start_service(&layout, &files.bin_exe)?;
    } else {
        print_manual_install(&layout, &files.bin_exe);
    }
    Ok(())
}

pub(crate) fn uninstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    with_data: bool,
    with_bin: bool,
    all: bool,
) -> Result<()> {
    require_system_scope(system)?;
    let layout = ServiceLayout::resolve(true, path_override, config_override)?;
    remove_service_definition()?;
    cleanup_service_layout(
        &layout,
        ServiceCleanupOptions {
            with_data: with_data || all,
            with_bin: with_bin || all,
            remove_manifest: all,
        },
    )
}

pub(crate) fn run_dispatcher(
    config: Option<PathBuf>,
    log_level: Option<DaemonLogLevel>,
) -> Result<()> {
    DISPATCHER_OPTIONS
        .set(DispatcherOptions { config, log_level })
        .map_err(|_| anyhow!("Windows service dispatcher was already configured"))?;
    service_dispatcher::start(service_name(), ffi_service_main)
        .context("start Windows service dispatcher")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        eprintln!("weather Windows service failed: {error:#}");
    }
}

fn run_service() -> Result<()> {
    let options = DISPATCHER_OPTIONS
        .get()
        .cloned()
        .context("Windows service options are not configured")?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let event_shutdown_tx = Arc::clone(&shutdown_tx);
    let event_handler = move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Ok(mut sender) = event_shutdown_tx.lock()
                && let Some(sender) = sender.take()
            {
                let _ = sender.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(service_name(), event_handler)
        .context("register Windows service control handler")?;
    set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        Duration::from_secs(10),
        ServiceExitCode::Win32(0),
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Windows service runtime")?;
    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
        Duration::ZERO,
        ServiceExitCode::Win32(0),
    )?;
    let result = runtime.block_on(run_until_service_stop(options, shutdown_rx, &status_handle));
    let exit_code = if result.is_ok() {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::Win32(1)
    };
    set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
        Duration::ZERO,
        exit_code,
    )?;
    result
}

async fn run_until_service_stop(
    options: DispatcherOptions,
    shutdown_rx: oneshot::Receiver<()>,
    status_handle: &ServiceStatusHandle,
) -> Result<()> {
    let config = options.config.clone();
    let engine = run(config.clone(), options.log_level, false, None);
    tokio::pin!(engine);
    tokio::select! {
        result = &mut engine => return result,
        _ = shutdown_rx => {}
    }
    set_status(
        status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        1,
        SERVICE_WAIT_TIMEOUT,
        ServiceExitCode::Win32(0),
    )?;
    let deadline = tokio::time::Instant::now() + SERVICE_WAIT_TIMEOUT;
    loop {
        tokio::select! {
            result = &mut engine => return result,
            stop_result = stop(config.clone()) => {
                if stop_result.is_ok() {
                    return tokio::time::timeout(SERVICE_WAIT_TIMEOUT, &mut engine)
                        .await
                        .context("weather engine did not stop before the Windows service timeout")?;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("weather engine was not ready to accept the Windows service stop request");
        }
        tokio::select! {
            result = &mut engine => return result,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

fn create_and_start_service(
    layout: &ServiceLayout,
    executable_path: &std::path::Path,
) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open Windows service manager")?;
    let service_info = ServiceInfo {
        name: OsString::from(service_name()),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable_path.to_path_buf(),
        launch_arguments: vec![
            OsString::from("daemon"),
            OsString::from("run"),
            OsString::from("--windows-service"),
            OsString::from("--config"),
            layout.config_path.as_os_str().to_owned(),
        ],
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(
            &service_info,
            ServiceAccess::CHANGE_CONFIG
                | ServiceAccess::START
                | ServiceAccess::STOP
                | ServiceAccess::QUERY_STATUS
                | ServiceAccess::DELETE,
        )
        .context("create weather Windows service")?;
    service
        .set_description(SERVICE_DESCRIPTION)
        .context("set weather Windows service description")?;
    match service.start::<&OsStr>(&[]) {
        Ok(()) => {}
        Err(error) if is_winapi_error(&error, ERROR_SERVICE_ALREADY_RUNNING) => {}
        Err(error) => return Err(error).context("start weather Windows service"),
    }
    wait_for_state(&service, ServiceState::Running, SERVICE_WAIT_TIMEOUT)?;
    println!("installed and started Windows service: {}", service_name());
    Ok(())
}

fn remove_service_definition() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Windows service manager")?;
    let service = match manager.open_service(
        service_name(),
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(error) if is_winapi_error(&error, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(error) => return Err(error).context("open weather Windows service"),
    };
    if service
        .query_status()
        .context("query weather Windows service")?
        .current_state
        != ServiceState::Stopped
    {
        match service.stop() {
            Ok(_) => {}
            Err(error) if is_winapi_error(&error, ERROR_SERVICE_NOT_ACTIVE) => {}
            Err(error) => return Err(error).context("stop weather Windows service"),
        }
        wait_for_state(&service, ServiceState::Stopped, SERVICE_WAIT_TIMEOUT)?;
    }
    service.delete().context("delete weather Windows service")?;
    drop(service);
    wait_for_deletion(&manager, SERVICE_WAIT_TIMEOUT)?;
    println!("removed Windows service: {}", service_name());
    Ok(())
}

fn wait_for_state(service: &Service, expected: ServiceState, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service
            .query_status()
            .context("query Windows service status")?;
        if status.current_state == expected {
            return Ok(());
        }
        if status.current_state == ServiceState::Stopped && expected != ServiceState::Stopped {
            bail!("Windows service stopped before reaching {expected:?}");
        }
        if Instant::now() >= deadline {
            bail!("Windows service did not reach {expected:?} before timeout");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_deletion(manager: &ServiceManager, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match manager.open_service(service_name(), ServiceAccess::QUERY_STATUS) {
            Err(error) if is_winapi_error(&error, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
            Ok(service) => drop(service),
            Err(error) => return Err(error).context("check Windows service deletion"),
        }
        if Instant::now() >= deadline {
            bail!("Windows service is still marked for deletion after timeout");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn set_status(
    status_handle: &ServiceStatusHandle,
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    wait_hint: Duration,
    exit_code: ServiceExitCode,
) -> Result<()> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state,
            controls_accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        })
        .context("report Windows service status")
}

fn require_system_scope(system: bool) -> Result<()> {
    if !system {
        bail!("Windows SCM services require --system")
    }
    Ok(())
}

fn is_winapi_error(error: &windows_service::Error, code: u32) -> bool {
    matches!(error, windows_service::Error::Winapi(error) if error.raw_os_error() == Some(code as i32))
}

fn print_manual_install(layout: &ServiceLayout, executable_path: &std::path::Path) {
    println!("installed Windows service files without modifying SCM");
    println!("binary: {}", executable_path.display());
    println!("config: {}", layout.config_path.display());
}
