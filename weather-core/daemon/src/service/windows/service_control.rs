use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::service::helper::{cleanup_base, install_service_files, service_name};

pub(crate) fn install(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
) -> Result<()> {
    let files = install_service_files(system, path_override, config_override)?;
    install_service_control(&files.bin_exe, &files.config_path, &files.base)
}

pub(crate) fn reinstall(
    system: bool,
    path_override: Option<PathBuf>,
    config_override: Option<PathBuf>,
    manage_service: bool,
) -> Result<()> {
    if cfg!(not(windows)) {
        bail!("windows backend requires windows");
    }
    let mut runner = ProcessCommandRunner;
    let mut logger = StdoutLogger;
    let installed = service_control_installed(&mut runner)?;
    if !installed {
        bail!(
            "{} windows service is not installed; run service install first",
            service_name()
        );
    }
    if !manage_service {
        let files = install_service_files(system, path_override, config_override)?;
        print_install_summary(&files.config_path, &files.base);
        let bin_dir = files
            .bin_exe
            .parent()
            .context("installed binary has no parent directory")?;
        for line in manual_reinstall_next_step_lines(bin_dir, &files.bin_exe, &files.config_path) {
            println!("{line}");
        }
        return Ok(());
    }
    reinstall_with(
        true,
        |runner, logger| {
            let files = install_service_files(system, path_override, config_override)?;
            create_service_control(&files.bin_exe, &files.config_path, runner, logger)?;
            print_install_summary(&files.config_path, &files.base);
            Ok(())
        },
        &mut runner,
        &mut logger,
    )
}

pub(crate) fn uninstall(with_data: bool, with_bin: bool) -> Result<()> {
    let _ = Command::new("sc").args(["stop", service_name()]).status();
    let _ = Command::new("sc").args(["delete", service_name()]).status();
    println!("removed service: {}", service_name());
    cleanup_base(with_data, with_bin)
}

fn install_service_control(bin_exe: &Path, config_path: &Path, base: &Path) -> Result<()> {
    if cfg!(not(windows)) {
        bail!("windows backend requires windows");
    }
    let bin_path = service_bin_path(bin_exe, config_path);
    let status = Command::new("sc")
        .args([
            "create",
            service_name(),
            "binPath=",
            &bin_path,
            "start=",
            "auto",
        ])
        .status()
        .context("failed to run sc create")?;
    if !status.success() {
        bail!("sc create failed with status {}", status);
    }
    println!("installed service: {}", service_name());
    print_install_summary(config_path, base);
    println!();
    println!("=== next steps ===");
    println!("bin PATH (run in cmd or PowerShell):");
    println!(
        "  setx PATH \"%PATH%;{}\"",
        bin_exe.parent().unwrap().display()
    );
    println!("start service:");
    println!("  sc start {}", service_name());
    println!("view status:");
    println!("  sc query {}", service_name());
    Ok(())
}

fn reinstall_with(
    installed: bool,
    install: impl FnOnce(&mut dyn CommandRunner, &mut dyn ServiceLogger) -> Result<()>,
    runner: &mut impl CommandRunner,
    logger: &mut impl ServiceLogger,
) -> Result<()> {
    if !installed {
        bail!(
            "{} windows service is not installed; run service install first",
            service_name()
        );
    }
    run_optional_sc(runner, logger, &["stop", service_name()]);
    require_sc(runner, logger, &["delete", service_name()])?;
    install(runner, logger)?;
    require_sc(runner, logger, &["start", service_name()])
}

fn service_control_installed(runner: &mut impl CommandRunner) -> Result<bool> {
    runner.status("sc", &["query", service_name()])
}

fn create_service_control(
    bin_exe: &Path,
    config_path: &Path,
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
) -> Result<()> {
    let bin_path = service_bin_path(bin_exe, config_path);
    require_sc(
        runner,
        logger,
        &[
            "create",
            service_name(),
            "binPath=",
            &bin_path,
            "start=",
            "auto",
        ],
    )?;
    println!("installed service: {}", service_name());
    Ok(())
}

fn service_bin_path(bin_exe: &Path, config_path: &Path) -> String {
    // sc create 需要 binPath 用引号包裹,参数也需转义。
    format!(
        "\"{}\" run --config \"{}\"",
        bin_exe.display(),
        config_path.display()
    )
}

fn print_install_summary(config_path: &Path, base: &Path) {
    println!("config: {}", config_path.display());
    println!("base:   {}", base.display());
}

fn manual_reinstall_next_step_lines(
    bin_dir: &Path,
    bin_exe: &Path,
    config_path: &Path,
) -> Vec<String> {
    let bin_path = service_bin_path(bin_exe, config_path);
    vec![
        String::new(),
        "=== next steps ===".to_string(),
        "bin PATH (run in cmd or PowerShell):".to_string(),
        format!("  setx PATH \"%PATH%;{}\"", bin_dir.display()),
        "stop service:".to_string(),
        format!("  sc stop {}", service_name()),
        "delete service:".to_string(),
        format!("  sc delete {}", service_name()),
        "create service:".to_string(),
        format!(
            "  sc create {} binPath= \"{}\" start= auto",
            service_name(),
            bin_path
        ),
        "start service:".to_string(),
        format!("  sc start {}", service_name()),
        "view status:".to_string(),
        format!("  sc query {}", service_name()),
    ]
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

fn sc_display(args: &[&str]) -> String {
    format!("sc {}", args.join(" "))
}

fn run_optional_sc(
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
    args: &[&str],
) {
    let command = sc_display(args);
    match runner.status("sc", args) {
        Ok(true) => logger.log(&format!("{command}: ok")),
        Ok(false) => logger.log(&format!("{command}: failed (ignored)")),
        Err(err) => logger.log(&format!("{command}: failed ({err}) (ignored)")),
    }
}

fn require_sc(
    runner: &mut (impl CommandRunner + ?Sized),
    logger: &mut (impl ServiceLogger + ?Sized),
    args: &[&str],
) -> Result<()> {
    let command = sc_display(args);
    match runner.status("sc", args) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reinstall_rejects_missing_service_without_side_effects() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();
        let installed = std::cell::Cell::new(false);

        let err = reinstall_with(
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
    fn reinstall_stops_deletes_installs_and_starts() {
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut runner = RecordingCommandRunner::new(events.clone());
        let mut logger = RecordingLogger::default();

        reinstall_with(
            true,
            |runner, logger| {
                events.borrow_mut().push("install".to_string());
                require_sc(runner, logger, &["create", service_name()])
            },
            &mut runner,
            &mut logger,
        )
        .unwrap();

        let expected_events = vec![
            format!("sc stop {}", service_name()),
            format!("sc delete {}", service_name()),
            "install".to_string(),
            format!("sc create {}", service_name()),
            format!("sc start {}", service_name()),
        ];
        assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
        let expected_logs = vec![
            format!("sc stop {}: ok", service_name()),
            format!("sc delete {}: ok", service_name()),
            format!("sc create {}: ok", service_name()),
            format!("sc start {}: ok", service_name()),
        ];
        assert_eq!(logger.messages.as_slice(), expected_logs.as_slice());
    }

    #[test]
    fn manual_reinstall_output_prints_service_control_next_steps() {
        let lines = manual_reinstall_next_step_lines(
            Path::new(r"C:\weather\bin"),
            Path::new(r"C:\weather\bin\weather-daemon.exe"),
            Path::new(r"C:\weather\config\weather.toml"),
        );
        let output = lines.join("\n");

        assert!(output.contains("=== next steps ==="));
        assert!(output.contains(&format!("sc stop {}", service_name())));
        assert!(output.contains(&format!("sc delete {}", service_name())));
        assert!(output.contains(&format!("sc create {}", service_name())));
        assert!(output.contains(&format!("sc start {}", service_name())));
    }

    struct RecordingCommandRunner {
        events: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl RecordingCommandRunner {
        fn new(events: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Self {
            Self { events }
        }
    }

    impl CommandRunner for RecordingCommandRunner {
        fn status(&mut self, program: &str, args: &[&str]) -> Result<bool> {
            self.events
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(true)
        }
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
