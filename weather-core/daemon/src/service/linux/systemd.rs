use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::service::helper::{
    ServiceCleanupOptions, ServiceLayout, cleanup_service_layout, install_service_files,
    service_name,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutputMode {
    Install,
    Reinstall,
    ManualInstall,
    ManualReinstall,
}

pub(crate) fn install(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    require_linux()?;
    let layout = ServiceLayout::resolve(system, path_override, config_override)?;
    if manage_service {
        let mut runner = ProcessCommandRunner;
        let mut logger = StdoutLogger;
        install_service_with_activation(
            &layout,
            InstallOutputMode::Install,
            &mut runner,
            &mut logger,
        )
    } else {
        install_files_and_unit(&layout, InstallOutputMode::ManualInstall)
    }
}

pub(crate) fn reinstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    require_linux()?;
    let layout = ServiceLayout::resolve(system, path_override, config_override)?;
    let unit_path = systemd_unit_path(system)?;
    let installed = unit_path
        .try_exists()
        .with_context(|| format!("inspect systemd unit {}", unit_path.display()))?;
    if !installed {
        bail!(
            "{} systemd service is not installed; run service install first",
            service_name()
        );
    }
    if !manage_service {
        return install_files_and_unit(&layout, InstallOutputMode::ManualReinstall);
    }
    let mut runner = ProcessCommandRunner;
    let mut logger = StdoutLogger;
    reinstall_with(
        system,
        true,
        |runner, logger| {
            install_service_with_activation(&layout, InstallOutputMode::Reinstall, runner, logger)
        },
        &mut runner,
        &mut logger,
    )
}

pub(crate) fn uninstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    with_data: bool,
    with_bin: bool,
    all: bool,
) -> Result<()> {
    require_linux()?;
    let layout = ServiceLayout::resolve(system, path_override, config_override)?;
    let unit_path = systemd_unit_path(system)?;
    let mut runner = ProcessCommandRunner;
    let mut logger = StdoutLogger;
    uninstall_with(
        &layout,
        &unit_path,
        ServiceCleanupOptions {
            with_data,
            with_bin,
            remove_manifest: all,
        },
        &mut runner,
        &mut logger,
    )
}

fn install_files_and_unit(layout: &ServiceLayout, output_mode: InstallOutputMode) -> Result<()> {
    let files = install_service_files(layout)?;
    install_unit(layout, &files.bin_exe, output_mode)
}

fn install_unit(
    layout: &ServiceLayout,
    bin_exe: &Path,
    output_mode: InstallOutputMode,
) -> Result<()> {
    require_linux()?;
    let unit_dir = if layout.system {
        PathBuf::from("/etc/systemd/system")
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".config/systemd/user")
    };
    fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join(format!("{}.service", service_name()));
    let unit = render_unit(layout, bin_exe);
    fs::write(&unit_path, unit)?;
    println!("installed unit: {}", unit_path.display());
    println!("config: {}", layout.config_path.display());
    println!("base:   {}", layout.base.display());
    let bin_dir = bin_exe
        .parent()
        .context("installed binary has no parent directory")?;
    for line in install_next_step_lines(layout.system, bin_dir, output_mode) {
        println!("{line}");
    }
    Ok(())
}

fn render_unit(layout: &ServiceLayout, bin_exe: &Path) -> String {
    let escaped_exe = shell_escape(&bin_exe.display().to_string());
    let escaped_config = shell_escape(&layout.config_path.display().to_string());
    let wanted_by = if layout.system {
        "multi-user.target"
    } else {
        "default.target"
    };
    let user_section = if layout.system {
        "\nUser=root\nGroup=root"
    } else {
        ""
    };
    format!(
        r#"[Unit]
Description=Weather Engine Daemon
After=network-online.target

[Service]
Type=simple
ExecStart={exe} daemon run --config {config}
Restart=on-failure
RestartSec=2s{user_section}

[Install]
WantedBy={wanted_by}
"#,
        exe = escaped_exe,
        config = escaped_config,
        user_section = user_section,
        wanted_by = wanted_by,
    )
}

fn install_next_step_lines(
    system: bool,
    bin_dir: &Path,
    output_mode: InstallOutputMode,
) -> Vec<String> {
    match output_mode {
        InstallOutputMode::Install => {
            let mut lines = vec![
                String::new(),
                "bin PATH (add to shell rc):".to_string(),
                format!("  export PATH=\"{}:$PATH\"", bin_dir.display()),
            ];
            if system {
                lines.extend([
                    "view logs:".to_string(),
                    format!("  sudo journalctl -u {} -f", service_name()),
                ]);
            } else {
                lines.extend([
                    "view logs:".to_string(),
                    format!("  journalctl --user -u {} -f", service_name()),
                    "linger (so user service starts at boot):".to_string(),
                    "  loginctl enable-linger".to_string(),
                ]);
            }
            lines
        }
        InstallOutputMode::Reinstall => Vec::new(),
        InstallOutputMode::ManualInstall | InstallOutputMode::ManualReinstall => {
            let mut lines = vec![
                String::new(),
                "=== next steps ===".to_string(),
                "bin PATH (add to shell rc):".to_string(),
                format!("  export PATH=\"{}:$PATH\"", bin_dir.display()),
            ];
            let systemctl = if system {
                "sudo systemctl".to_string()
            } else {
                "systemctl --user".to_string()
            };
            if output_mode == InstallOutputMode::ManualReinstall {
                lines.extend([
                    "stop service:".to_string(),
                    format!("  {systemctl} stop {}", service_name()),
                ]);
            }
            lines.extend([
                "enable & start service:".to_string(),
                format!("  {systemctl} daemon-reload"),
                format!("  {systemctl} enable --now {}", service_name()),
                "view logs:".to_string(),
            ]);
            if system {
                lines.push(format!("  journalctl -u {} -f", service_name()));
            } else {
                lines.extend([
                    format!("  journalctl --user -u {} -f", service_name()),
                    "linger (so user service starts at boot):".to_string(),
                    "  loginctl enable-linger".to_string(),
                ]);
            }
            lines
        }
    }
}

fn install_service_with_activation(
    layout: &ServiceLayout,
    output_mode: InstallOutputMode,
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
) -> Result<()> {
    install_files_and_unit(layout, output_mode)?;
    activate_service(layout.system, runner, logger)
}

fn activate_service(
    system: bool,
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
) -> Result<()> {
    require_systemctl(runner, logger, system, &["daemon-reload"])?;
    require_systemctl(runner, logger, system, &["enable", "--now", service_name()])?;
    Ok(())
}

fn reinstall_with(
    system: bool,
    installed: bool,
    install: impl FnOnce(&mut dyn CommandRunner, &mut dyn ServiceLogger) -> Result<()>,
    runner: &mut impl CommandRunner,
    logger: &mut impl ServiceLogger,
) -> Result<()> {
    if !installed {
        bail!(
            "{} systemd service is not installed; run service install first",
            service_name()
        );
    }
    require_systemctl(runner, logger, system, &["stop", service_name()])?;
    install(runner, logger)
}

fn uninstall_with(
    layout: &ServiceLayout,
    unit_path: &Path,
    cleanup_options: ServiceCleanupOptions,
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
) -> Result<()> {
    remove_unit(layout.system, unit_path, runner, logger)?;
    cleanup_service_layout(layout, cleanup_options)
}

fn remove_unit(
    system: bool,
    unit_path: &Path,
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
) -> Result<()> {
    if !unit_path
        .try_exists()
        .with_context(|| format!("inspect systemd unit {}", unit_path.display()))?
    {
        return Ok(());
    }
    require_systemctl(runner, logger, system, &["stop", service_name()])?;
    require_systemctl(runner, logger, system, &["disable", service_name()])?;
    match fs::remove_file(unit_path) {
        Ok(()) => println!("removed unit: {}", unit_path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("remove systemd unit {}", unit_path.display()));
        }
    }
    require_systemctl(runner, logger, system, &["daemon-reload"])?;
    Ok(())
}

fn user_unit_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(format!(".config/systemd/user/{}.service", service_name())))
}

fn systemd_unit_path(system: bool) -> Result<PathBuf> {
    if system {
        Ok(PathBuf::from(format!(
            "/etc/systemd/system/{}.service",
            service_name()
        )))
    } else {
        user_unit_path()
    }
}

fn require_linux() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("systemd service backend is supported only on Linux");
    }
    Ok(())
}

trait CommandRunner {
    fn status(&mut self, program: &str, args: &[&str]) -> Result<bool>;
}

trait ServiceLogger {
    fn log(&mut self, message: &str);
}

struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn status(&mut self, program: &str, args: &[&str]) -> Result<bool> {
        let status = Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("failed to run {program} {}", args.join(" ")))?;
        Ok(status.success())
    }
}

struct StdoutLogger;

impl ServiceLogger for StdoutLogger {
    fn log(&mut self, message: &str) {
        println!("{message}");
    }
}

fn systemctl_args<'a>(system: bool, args: &'a [&'a str]) -> Vec<&'a str> {
    let mut full_args = Vec::new();
    if !system {
        full_args.push("--user");
    }
    full_args.extend_from_slice(args);
    full_args
}

fn systemctl_display(system: bool, args: &[&str]) -> String {
    let full_args = systemctl_args(system, args);
    format!("systemctl {}", full_args.join(" "))
}

fn run_systemctl(
    runner: &mut (impl CommandRunner + ?Sized),
    system: bool,
    args: &[&str],
) -> Result<bool> {
    let full_args = systemctl_args(system, args);
    runner.status("systemctl", &full_args)
}

fn require_systemctl(
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
    system: bool,
    args: &[&str],
) -> Result<()> {
    let command = systemctl_display(system, args);
    match run_systemctl(runner, system, args) {
        Ok(true) => {
            logger.log(&format!("{command}: ok"));
            Ok(())
        }
        Ok(false) => {
            logger.log(&format!("{command}: failed"));
            bail!("{command} failed")
        }
        Err(err) => {
            logger.log(&format!("{command}: failed ({err})"));
            Err(err).with_context(|| format!("{command} failed"))
        }
    }
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_unit_uses_the_multicall_daemon_subcommand() {
        let layout = ServiceLayout::resolve(
            false,
            Some(PathBuf::from("/tmp/weather")),
            Some(PathBuf::from("/tmp/weather/config/weather.toml")),
        )
        .unwrap();

        let unit = render_unit(&layout, Path::new("/tmp/weather/bin/weather.app"));

        assert!(unit.contains("weather.app"));
        assert!(unit.contains(" daemon run --config "));
        assert!(unit.contains("weather.toml"));
        assert_eq!(service_name(), "weather-daemon");
    }
    use std::{
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn reinstall_rejects_missing_unit_without_side_effects() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();
        let installed = std::cell::Cell::new(false);

        let err = reinstall_with(
            false,
            false,
            |_, _| {
                installed.set(true);
                Ok(())
            },
            &mut runner,
            &mut logger,
        )
        .unwrap_err();

        assert!(err.to_string().contains("not installed"));
        assert!(!installed.get());
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn reinstall_user_stops_installs_reloads_and_starts() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        reinstall_with(
            false,
            true,
            |runner, logger| {
                events.borrow_mut().push("install".to_string());
                activate_service(false, runner, logger)
            },
            &mut runner,
            &mut logger,
        )
        .unwrap();

        let expected_events = vec![
            format!("systemctl --user stop {}", service_name()),
            "install".to_string(),
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable --now {}", service_name()),
        ];
        assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
        let expected_logs = vec![
            format!("systemctl --user stop {}: ok", service_name()),
            "systemctl --user daemon-reload: ok".to_string(),
            format!("systemctl --user enable --now {}: ok", service_name()),
        ];
        assert_eq!(logger.messages.as_slice(), expected_logs.as_slice());
    }

    #[test]
    fn reinstall_system_omits_user_flag() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        reinstall_with(
            true,
            true,
            |runner, logger| {
                events.borrow_mut().push("install".to_string());
                activate_service(true, runner, logger)
            },
            &mut runner,
            &mut logger,
        )
        .unwrap();

        let expected_events = vec![
            format!("systemctl stop {}", service_name()),
            "install".to_string(),
            "systemctl daemon-reload".to_string(),
            format!("systemctl enable --now {}", service_name()),
        ];
        assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
        let expected_logs = vec![
            format!("systemctl stop {}: ok", service_name()),
            "systemctl daemon-reload: ok".to_string(),
            format!("systemctl enable --now {}: ok", service_name()),
        ];
        assert_eq!(logger.messages.as_slice(), expected_logs.as_slice());
    }

    #[test]
    fn reinstall_stop_failure_prevents_install() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stop = format!("systemctl --user stop {}", service_name());
        let mut runner = RecordingCommandRunner::fail_on(events.clone(), stop.clone());
        let mut logger = RecordingLogger::default();
        let installed = std::cell::Cell::new(false);

        let err = reinstall_with(
            false,
            true,
            |_, _| {
                installed.set(true);
                Ok(())
            },
            &mut runner,
            &mut logger,
        )
        .unwrap_err();

        assert!(err.to_string().contains(&format!("{stop} failed")));
        assert!(!installed.get());
        assert_eq!(events.borrow().as_slice(), std::slice::from_ref(&stop));
        assert_eq!(logger.messages, vec![format!("{stop}: failed")]);
    }

    #[test]
    fn install_service_reloads_enables_and_starts() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        activate_service(false, &mut runner, &mut logger).unwrap();

        let expected_events = vec![
            "systemctl --user daemon-reload".to_string(),
            format!("systemctl --user enable --now {}", service_name()),
        ];
        assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
        let expected_logs = vec![
            "systemctl --user daemon-reload: ok".to_string(),
            format!("systemctl --user enable --now {}: ok", service_name()),
        ];
        assert_eq!(logger.messages.as_slice(), expected_logs.as_slice());
    }

    #[test]
    fn remove_unit_stops_disables_removes_and_reloads() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "weather-remove-unit-test-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&base).unwrap();
        let unit = base.join(format!("{}.service", service_name()));
        fs::write(&unit, b"unit").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        remove_unit(false, &unit, &mut runner, &mut logger).unwrap();

        assert!(!unit.exists());
        let expected_events = vec![
            format!("systemctl --user stop {}", service_name()),
            format!("systemctl --user disable {}", service_name()),
            "systemctl --user daemon-reload".to_string(),
        ];
        assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
        let expected_logs = vec![
            format!("systemctl --user stop {}: ok", service_name()),
            format!("systemctl --user disable {}: ok", service_name()),
            "systemctl --user daemon-reload: ok".to_string(),
        ];
        assert_eq!(logger.messages.as_slice(), expected_logs.as_slice());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_unit_stop_failure_preserves_the_unit() {
        let base = unique_test_dir("remove-stop-failure");
        fs::create_dir_all(&base).unwrap();
        let unit = base.join("weather.service");
        fs::write(&unit, b"unit").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stop = format!("systemctl --user stop {}", service_name());
        let mut runner = RecordingCommandRunner::fail_on(events.clone(), stop.clone());
        let mut logger = RecordingLogger::default();

        let err = remove_unit(false, &unit, &mut runner, &mut logger).unwrap_err();

        assert!(err.to_string().contains(&format!("{stop} failed")));
        assert!(unit.exists());
        assert_eq!(events.borrow().as_slice(), &[stop]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_unit_disable_failure_preserves_the_unit() {
        let base = unique_test_dir("remove-disable-failure");
        fs::create_dir_all(&base).unwrap();
        let unit = base.join("weather.service");
        fs::write(&unit, b"unit").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let disable = format!("systemctl --user disable {}", service_name());
        let mut runner = RecordingCommandRunner::fail_on(events.clone(), disable.clone());
        let mut logger = RecordingLogger::default();

        let err = remove_unit(false, &unit, &mut runner, &mut logger).unwrap_err();

        assert!(err.to_string().contains(&format!("{disable} failed")));
        assert!(unit.exists());
        assert_eq!(
            events.borrow().as_slice(),
            &[format!("systemctl --user stop {}", service_name()), disable,]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_unit_reload_failure_is_reported() {
        let base = unique_test_dir("remove-reload-failure");
        fs::create_dir_all(&base).unwrap();
        let unit = base.join("weather.service");
        fs::write(&unit, b"unit").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let reload = "systemctl --user daemon-reload".to_string();
        let mut runner = RecordingCommandRunner::fail_on(events.clone(), reload.clone());
        let mut logger = RecordingLogger::default();

        let err = remove_unit(false, &unit, &mut runner, &mut logger).unwrap_err();

        assert!(err.to_string().contains(&format!("{reload} failed")));
        assert!(!unit.exists());
        assert_eq!(events.borrow().last(), Some(&reload));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_unit_still_cleans_the_selected_layout() {
        let base = unique_test_dir("missing-unit-cleanup");
        let layout = ServiceLayout::resolve(
            false,
            Some(base.join("base")),
            Some(base.join("external/weather.toml")),
        )
        .unwrap();
        fs::create_dir_all(layout.config_path.parent().unwrap()).unwrap();
        fs::write(&layout.config_path, b"config").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        uninstall_with(
            &layout,
            &base.join("missing.service"),
            ServiceCleanupOptions {
                with_data: true,
                with_bin: false,
                remove_manifest: false,
            },
            &mut runner,
            &mut logger,
        )
        .unwrap();

        assert!(!layout.config_path.exists());
        assert!(events.borrow().is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn uninstall_removes_only_the_selected_scope_unit() {
        let base = unique_test_dir("selected-unit");
        fs::create_dir_all(&base).unwrap();
        let layout = ServiceLayout::resolve(false, Some(base.join("layout")), None).unwrap();
        let selected_unit = base.join("user.service");
        let other_unit = base.join("system.service");
        fs::write(&selected_unit, b"user").unwrap();
        fs::write(&other_unit, b"system").unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        uninstall_with(
            &layout,
            &selected_unit,
            ServiceCleanupOptions {
                with_data: false,
                with_bin: false,
                remove_manifest: false,
            },
            &mut runner,
            &mut logger,
        )
        .unwrap();

        assert!(!selected_unit.exists());
        assert!(other_unit.exists());
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| event.starts_with("systemctl --user "))
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn uninstall_command_failure_skips_component_cleanup() {
        for command in [
            format!("systemctl --user stop {}", service_name()),
            format!("systemctl --user disable {}", service_name()),
            "systemctl --user daemon-reload".to_string(),
        ] {
            let base = unique_test_dir("failed-uninstall-cleanup");
            fs::create_dir_all(&base).unwrap();
            let layout = ServiceLayout::resolve(
                false,
                Some(base.join("layout")),
                Some(base.join("weather.toml")),
            )
            .unwrap();
            fs::write(&layout.config_path, b"config").unwrap();
            let unit = base.join("weather.service");
            fs::write(&unit, b"unit").unwrap();
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut runner = RecordingCommandRunner::fail_on(events, command);
            let mut logger = RecordingLogger::default();

            uninstall_with(
                &layout,
                &unit,
                ServiceCleanupOptions {
                    with_data: true,
                    with_bin: false,
                    remove_manifest: false,
                },
                &mut runner,
                &mut logger,
            )
            .unwrap_err();

            assert!(layout.config_path.exists());
            let _ = fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn reinstall_output_omits_systemctl_next_steps() {
        let lines = install_next_step_lines(
            false,
            Path::new("/home/rei/.weather/bin"),
            InstallOutputMode::Reinstall,
        );
        let output = lines.join("\n");

        assert!(!output.contains("systemctl --user daemon-reload"));
        assert!(!output.contains(&format!("systemctl --user enable --now {}", service_name())));
        assert!(!output.contains("enable & start service"));
    }

    #[test]
    fn manual_install_output_prints_systemctl_next_steps() {
        let lines = install_next_step_lines(
            false,
            Path::new("/home/rei/.weather/bin"),
            InstallOutputMode::ManualInstall,
        );
        let output = lines.join("\n");

        assert!(output.contains("=== next steps ==="));
        assert!(output.contains("systemctl --user daemon-reload"));
        assert!(output.contains(&format!("systemctl --user enable --now {}", service_name())));
    }

    #[test]
    fn manual_reinstall_output_prints_stop_step() {
        let lines = install_next_step_lines(
            false,
            Path::new("/home/rei/.weather/bin"),
            InstallOutputMode::ManualReinstall,
        );
        let output = lines.join("\n");

        assert!(output.contains(&format!("systemctl --user stop {}", service_name())));
        assert!(output.contains(&format!("systemctl --user enable --now {}", service_name())));
    }

    struct RecordingCommandRunner {
        events: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        fail_on: Option<String>,
    }

    impl RecordingCommandRunner {
        fn new(events: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Self {
            Self {
                events,
                fail_on: None,
            }
        }

        fn fail_on(events: std::rc::Rc<std::cell::RefCell<Vec<String>>>, command: String) -> Self {
            Self {
                events,
                fail_on: Some(command),
            }
        }
    }

    impl CommandRunner for RecordingCommandRunner {
        fn status(&mut self, program: &str, args: &[&str]) -> Result<bool> {
            let command = format!("{program} {}", args.join(" "));
            self.events.borrow_mut().push(command.clone());
            Ok(self.fail_on.as_deref() != Some(command.as_str()))
        }
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "weather-systemd-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[derive(Default)]
    struct RecordingLogger {
        messages: Vec<String>,
    }

    impl ServiceLogger for RecordingLogger {
        fn log(&mut self, message: &str) {
            self.messages.push(message.to_string());
        }
    }
}
