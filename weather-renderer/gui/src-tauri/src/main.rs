use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal as _},
    process::ExitCode,
};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Daemon,
    Tui,
    Gui,
}

#[derive(Debug, PartialEq, Eq)]
struct Invocation {
    mode: Mode,
    args: Vec<OsString>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("weather.app: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = std::env::args_os().collect::<Vec<_>>();
    let invocation = dispatch(&args, interactive_launch());
    execute(invocation)
}

fn dispatch(args: &[OsString], interactive: bool) -> Invocation {
    let alias = args
        .first()
        .and_then(|arg| executable_alias(arg))
        .and_then(alias_mode);
    let mut forwarded = args.iter().skip(1).cloned().collect::<Vec<_>>();
    let mode = if let Some(mode) = alias {
        mode
    } else if let Some(mode) = forwarded.first().and_then(selector_mode) {
        forwarded.remove(0);
        mode
    } else if interactive {
        Mode::Tui
    } else {
        Mode::Gui
    };
    Invocation {
        mode,
        args: forwarded,
    }
}

fn executable_alias(arg: &OsStr) -> Option<String> {
    let raw = arg.to_str()?;
    let file_name = raw
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())?
        .to_ascii_lowercase();
    Some(
        file_name
            .strip_suffix(".exe")
            .unwrap_or(&file_name)
            .to_string(),
    )
}

fn alias_mode(alias: String) -> Option<Mode> {
    match alias.as_str() {
        "weather-daemon" => Some(Mode::Daemon),
        "weather-tui" => Some(Mode::Tui),
        "weather-gui" => Some(Mode::Gui),
        _ => None,
    }
}

fn selector_mode(arg: &OsString) -> Option<Mode> {
    match arg.to_str()? {
        "daemon" => Some(Mode::Daemon),
        "tui" => Some(Mode::Tui),
        "gui" => Some(Mode::Gui),
        _ => None,
    }
}

fn execute(invocation: Invocation) -> Result<()> {
    match invocation.mode {
        Mode::Daemon => run_daemon(invocation.args),
        Mode::Tui => run_tui(invocation.args),
        Mode::Gui => run_gui(),
    }
}

#[cfg(feature = "daemon")]
fn run_daemon(args: Vec<OsString>) -> Result<()> {
    let args = component_args("weather-daemon", args);
    runtime()?.block_on(weather_daemon::run_from(args))
}

#[cfg(not(feature = "daemon"))]
fn run_daemon(_args: Vec<OsString>) -> Result<()> {
    Err(anyhow::anyhow!(
        "daemon support was not compiled; rebuild with --features daemon"
    ))
}

#[cfg(feature = "tui")]
fn run_tui(args: Vec<OsString>) -> Result<()> {
    let args = component_args("weather-tui", args);
    runtime()?.block_on(weather_tui::run_from(
        args,
        weather_tui::RunOptions {
            embedded_daemon: cfg!(feature = "daemon"),
        },
    ))
}

#[cfg(not(feature = "tui"))]
fn run_tui(_args: Vec<OsString>) -> Result<()> {
    Err(anyhow::anyhow!(
        "TUI support was not compiled; rebuild with --features tui"
    ))
}

#[cfg(feature = "gui")]
fn run_gui() -> Result<()> {
    hide_desktop_console();
    weather_app_lib::run();
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn run_gui() -> Result<()> {
    Err(anyhow::anyhow!(
        "GUI support was not compiled; rebuild with --features gui"
    ))
}

#[cfg(any(feature = "daemon", feature = "tui", test))]
fn component_args(program: &str, args: Vec<OsString>) -> Vec<OsString> {
    std::iter::once(OsString::from(program))
        .chain(args)
        .collect()
}

#[cfg(any(feature = "daemon", feature = "tui"))]
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

#[cfg(not(windows))]
fn interactive_launch() -> bool {
    io::stdin().is_terminal() || io::stdout().is_terminal() || io::stderr().is_terminal()
}

#[cfg(windows)]
fn interactive_launch() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    if !(io::stdin().is_terminal() || io::stdout().is_terminal() || io::stderr().is_terminal()) {
        return false;
    }
    let mut processes = [0_u32; 2];
    unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) > 1 }
}

#[cfg(all(feature = "gui", windows))]
fn hide_desktop_console() {
    use windows_sys::Win32::{
        System::Console::GetConsoleWindow,
        UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
    };

    let window = unsafe { GetConsoleWindow() };
    if !window.is_null() {
        unsafe {
            ShowWindow(window, SW_HIDE);
        }
    }
}

#[cfg(all(feature = "gui", not(windows)))]
fn hide_desktop_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn explicit_selectors_override_launch_environment() {
        assert_eq!(
            dispatch(&args(&["weather.app", "daemon", "probe"]), true),
            Invocation {
                mode: Mode::Daemon,
                args: args(&["probe"]),
            }
        );
        assert_eq!(
            dispatch(&args(&["weather.app", "gui"]), true).mode,
            Mode::Gui
        );
        assert_eq!(
            dispatch(&args(&["weather.app", "tui"]), false).mode,
            Mode::Tui
        );
    }

    #[test]
    fn busybox_aliases_select_components() {
        assert_eq!(
            dispatch(&args(&["/usr/bin/weather-daemon", "status"]), false),
            Invocation {
                mode: Mode::Daemon,
                args: args(&["status"]),
            }
        );
        assert_eq!(
            dispatch(&args(&["C:\\Weather\\weather-tui.exe"]), false).mode,
            Mode::Tui
        );
        assert_eq!(dispatch(&args(&["weather-gui"]), true).mode, Mode::Gui);
    }

    #[test]
    fn launch_environment_selects_default_frontend() {
        assert_eq!(dispatch(&args(&["weather.app"]), true).mode, Mode::Tui);
        assert_eq!(dispatch(&args(&["weather.app"]), false).mode, Mode::Gui);
    }

    #[test]
    fn component_argument_lists_have_compatible_argv_zero() {
        assert_eq!(
            component_args("weather-daemon", args(&["probe"])),
            args(&["weather-daemon", "probe"])
        );
    }
}
