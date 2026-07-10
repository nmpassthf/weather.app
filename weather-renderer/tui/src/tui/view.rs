use ratatui::Frame;

use super::TuiApp;

pub(super) fn render(frame: &mut Frame<'_>, app: &mut TuiApp) {
    super::draw(frame, app);
}
