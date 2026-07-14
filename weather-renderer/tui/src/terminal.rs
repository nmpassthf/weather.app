use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};

trait TerminalOps {
    type Terminal;

    fn enable_raw_mode(&mut self) -> Result<()>;
    fn enter_alternate_screen(&mut self) -> Result<()>;
    fn create_terminal(&mut self) -> Result<Self::Terminal>;
    fn show_cursor(&mut self, terminal: &mut Self::Terminal) -> Result<()>;
    fn leave_alternate_screen(&mut self, terminal: Option<&mut Self::Terminal>) -> Result<()>;
    fn disable_raw_mode(&mut self) -> Result<()>;
}

struct TerminalLifecycle<Ops: TerminalOps> {
    ops: Ops,
    terminal: Option<Ops::Terminal>,
    raw_enabled: bool,
    alternate_entered: bool,
    terminal_created: bool,
}

impl<Ops: TerminalOps> TerminalLifecycle<Ops> {
    fn start(ops: Ops) -> Result<Self> {
        let mut lifecycle = Self {
            ops,
            terminal: None,
            raw_enabled: false,
            alternate_entered: false,
            terminal_created: false,
        };

        lifecycle.ops.enable_raw_mode()?;
        lifecycle.raw_enabled = true;

        lifecycle.ops.enter_alternate_screen()?;
        lifecycle.alternate_entered = true;

        let terminal = lifecycle.ops.create_terminal()?;
        lifecycle.terminal = Some(terminal);
        lifecycle.terminal_created = true;

        Ok(lifecycle)
    }

    fn restore(&mut self) -> Result<()> {
        let mut first_error = None;

        if std::mem::take(&mut self.terminal_created) {
            debug_assert!(self.terminal.is_some());
            if let Some(terminal) = self.terminal.as_mut() {
                remember_first_error(&mut first_error, self.ops.show_cursor(terminal));
            }
        }
        if std::mem::take(&mut self.alternate_entered) {
            remember_first_error(
                &mut first_error,
                self.ops.leave_alternate_screen(self.terminal.as_mut()),
            );
        }
        if std::mem::take(&mut self.raw_enabled) {
            remember_first_error(&mut first_error, self.ops.disable_raw_mode());
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<Ops: TerminalOps> Drop for TerminalLifecycle<Ops> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn remember_first_error(first_error: &mut Option<anyhow::Error>, result: Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

struct CrosstermOps;

type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;

impl TerminalOps for CrosstermOps {
    type Terminal = CrosstermTerminal;

    fn enable_raw_mode(&mut self) -> Result<()> {
        enable_raw_mode()?;
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(())
    }

    fn create_terminal(&mut self) -> Result<Self::Terminal> {
        Ok(Terminal::new(CrosstermBackend::new(io::stdout()))?)
    }

    fn show_cursor(&mut self, terminal: &mut Self::Terminal) -> Result<()> {
        terminal.show_cursor()?;
        Ok(())
    }

    fn leave_alternate_screen(&mut self, terminal: Option<&mut Self::Terminal>) -> Result<()> {
        match terminal {
            Some(terminal) => execute!(terminal.backend_mut(), LeaveAlternateScreen)?,
            None => execute!(io::stdout(), LeaveAlternateScreen)?,
        }
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> Result<()> {
        disable_raw_mode()?;
        Ok(())
    }
}

pub(crate) struct TerminalGuard {
    lifecycle: TerminalLifecycle<CrosstermOps>,
}

impl TerminalGuard {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            lifecycle: TerminalLifecycle::start(CrosstermOps)?,
        })
    }

    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.lifecycle
            .terminal
            .as_mut()
            .expect("terminal lifecycle must own a constructed terminal")
            .draw(render)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use anyhow::bail;

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Call {
        EnableRaw,
        EnterAlternate,
        CreateTerminal,
        ShowCursor,
        LeaveAlternate,
        DisableRaw,
    }

    struct FakeOps {
        calls: Rc<RefCell<Vec<Call>>>,
        fail_at: Option<Call>,
    }

    impl FakeOps {
        fn call(&self, call: Call) -> Result<()> {
            self.calls.borrow_mut().push(call);
            if self.fail_at == Some(call) {
                bail!("injected {call:?} failure");
            }
            Ok(())
        }
    }

    impl TerminalOps for FakeOps {
        type Terminal = ();

        fn enable_raw_mode(&mut self) -> Result<()> {
            self.call(Call::EnableRaw)
        }

        fn enter_alternate_screen(&mut self) -> Result<()> {
            self.call(Call::EnterAlternate)
        }

        fn create_terminal(&mut self) -> Result<Self::Terminal> {
            self.call(Call::CreateTerminal)
        }

        fn show_cursor(&mut self, _terminal: &mut Self::Terminal) -> Result<()> {
            self.call(Call::ShowCursor)
        }

        fn leave_alternate_screen(&mut self, _terminal: Option<&mut Self::Terminal>) -> Result<()> {
            self.call(Call::LeaveAlternate)
        }

        fn disable_raw_mode(&mut self) -> Result<()> {
            self.call(Call::DisableRaw)
        }
    }

    fn fake(fail_at: Option<Call>) -> (FakeOps, Rc<RefCell<Vec<Call>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        (
            FakeOps {
                calls: calls.clone(),
                fail_at,
            },
            calls,
        )
    }

    #[test]
    fn raw_mode_failure_has_nothing_to_restore() {
        let (ops, calls) = fake(Some(Call::EnableRaw));

        assert!(TerminalLifecycle::start(ops).is_err());

        assert_eq!(*calls.borrow(), vec![Call::EnableRaw]);
    }

    #[test]
    fn alternate_screen_failure_restores_raw_mode() {
        let (ops, calls) = fake(Some(Call::EnterAlternate));

        assert!(TerminalLifecycle::start(ops).is_err());

        assert_eq!(
            *calls.borrow(),
            vec![Call::EnableRaw, Call::EnterAlternate, Call::DisableRaw]
        );
    }

    #[test]
    fn terminal_creation_failure_leaves_alternate_and_raw_modes() {
        let (ops, calls) = fake(Some(Call::CreateTerminal));

        assert!(TerminalLifecycle::start(ops).is_err());

        assert_eq!(
            *calls.borrow(),
            vec![
                Call::EnableRaw,
                Call::EnterAlternate,
                Call::CreateTerminal,
                Call::LeaveAlternate,
                Call::DisableRaw,
            ]
        );
    }

    #[test]
    fn successful_restore_is_ordered_and_runs_only_once() {
        let (ops, calls) = fake(None);
        let mut lifecycle = TerminalLifecycle::start(ops).unwrap();

        lifecycle.restore().unwrap();
        lifecycle.restore().unwrap();
        drop(lifecycle);

        assert_eq!(
            *calls.borrow(),
            vec![
                Call::EnableRaw,
                Call::EnterAlternate,
                Call::CreateTerminal,
                Call::ShowCursor,
                Call::LeaveAlternate,
                Call::DisableRaw,
            ]
        );
    }

    #[test]
    fn drop_attempts_all_cleanup_when_leaving_alternate_fails() {
        let (ops, calls) = fake(Some(Call::LeaveAlternate));

        drop(TerminalLifecycle::start(ops).unwrap());

        assert_eq!(
            *calls.borrow(),
            vec![
                Call::EnableRaw,
                Call::EnterAlternate,
                Call::CreateTerminal,
                Call::ShowCursor,
                Call::LeaveAlternate,
                Call::DisableRaw,
            ]
        );
    }

    #[test]
    fn every_cleanup_failure_still_attempts_later_steps_and_remains_idempotent() {
        for failure in [Call::ShowCursor, Call::LeaveAlternate, Call::DisableRaw] {
            let (ops, calls) = fake(Some(failure));
            let mut lifecycle = TerminalLifecycle::start(ops).unwrap();

            let error = lifecycle.restore().unwrap_err();
            assert!(error.to_string().contains(&format!("{failure:?}")));
            lifecycle.restore().unwrap();
            drop(lifecycle);

            assert_eq!(
                *calls.borrow(),
                vec![
                    Call::EnableRaw,
                    Call::EnterAlternate,
                    Call::CreateTerminal,
                    Call::ShowCursor,
                    Call::LeaveAlternate,
                    Call::DisableRaw,
                ]
            );
        }
    }
}
