use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
    },
};
use weather_schema::{ConfiguredStation, EngineStatus};

use crate::presentation::{AlertView, CurrentWeatherView, ForecastView, WeatherView};

use super::{
    SEARCH_PAGE_SIZE, TuiApp,
    state::{InputMode, PanelFocus, SearchStatus},
};

pub(super) fn render(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let chunks = main_layout(frame.area());
    let weather = app.snapshot.as_ref().map(WeatherView::new);

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app, weather.as_ref());
    draw_footer(frame, chunks[2], app);

    match app.mode {
        InputMode::Manage | InputMode::ManageMove => {
            draw_manage(frame, centered_rect(80, 70, frame.area()), app);
        }
        InputMode::ManageAddSearch | InputMode::ManageBrowseSearch => {
            draw_search(frame, centered_rect(72, 70, frame.area()), app);
        }
        InputMode::About => {
            draw_about(frame, centered_rect(60, 70, frame.area()), app);
        }
        InputMode::Normal => {}
    }
}

fn main_layout(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(4),
        ])
        .split(area)
        .to_vec()
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let title = if let Some(station) = app.snapshot.as_ref().and_then(|s| s.station.as_ref()) {
        if station.name.is_empty() {
            format!("{} {}", station.province, station.city)
        } else {
            station.name.clone()
        }
    } else {
        "Weather".to_string()
    };
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL).title("TUI")),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp, weather: Option<&WeatherView>) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(50)])
        .split(area);
    draw_stations(frame, columns[0], app);

    let has_alert = weather.and_then(|view| view.alert.as_ref()).is_some();
    let right = right_panel_layout(columns[1], has_alert);
    draw_current(
        frame,
        right[0],
        weather.and_then(|view| view.current.as_ref()),
        app.focus,
    );
    draw_forecast(
        frame,
        right[1],
        weather.and_then(|view| view.forecast.as_ref()),
        app.focus,
        app.forecast_scroll,
    );
    if has_alert {
        draw_alert(
            frame,
            right[2],
            weather.and_then(|view| view.alert.as_ref()),
            app.focus,
            app.alert_scroll,
        );
    }
}

fn right_panel_layout(area: Rect, has_alert: bool) -> Vec<Rect> {
    let constraints = if has_alert {
        vec![
            Constraint::Length(8),
            Constraint::Min(2),
            Constraint::Min(3),
        ]
    } else {
        vec![Constraint::Length(8), Constraint::Min(2)]
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

fn focused_block(title: &str, focus: PanelFocus, target: PanelFocus) -> Block<'_> {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    if focus == target {
        block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        block
    }
}

fn draw_stations(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let visible: Vec<(usize, &ConfiguredStation)> = app
        .stations
        .iter()
        .enumerate()
        .filter(|(_, station)| !app.hidden_stations.contains(&station.name))
        .collect();
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new("All stations hidden (m to manage)").block(focused_block(
                "Stations",
                app.focus,
                PanelFocus::Stations,
            )),
            area,
        );
        return;
    }
    let items = visible
        .iter()
        .map(|(index, station)| {
            let marker = if station.enabled { "[x]" } else { "[ ]" };
            ListItem::new(format!("{:>2}. {marker} {}", index + 1, station.name))
        })
        .collect::<Vec<_>>();
    let visible_pos = visible
        .iter()
        .position(|(index, _)| Some(*index) == app.selected_station);
    let mut state = ListState::default();
    state.select(visible_pos);
    frame.render_stateful_widget(
        List::new(items)
            .block(focused_block("Stations", app.focus, PanelFocus::Stations))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        area,
        &mut state,
    );
}

fn draw_current(
    frame: &mut Frame<'_>,
    area: Rect,
    current: Option<&CurrentWeatherView>,
    focus: PanelFocus,
) {
    let rows = current_rows(current).into_iter().map(|(label, value)| {
        Row::new(vec![
            Cell::from(label).style(Style::default().fg(Color::Yellow)),
            Cell::from(value),
        ])
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(14), Constraint::Min(10)]).block(focused_block(
            "Current",
            focus,
            PanelFocus::Current,
        )),
        area,
    );
}

fn current_rows(current: Option<&CurrentWeatherView>) -> Vec<(&'static str, String)> {
    if let Some(current) = current {
        vec![
            ("Weather", current.info.clone()),
            ("Temperature", temperature_summary(current)),
            ("Feels like", current.feel_temperature.clone()),
            (
                "Humidity",
                format!(
                    "{}  rain {}  pressure {}",
                    current.humidity, current.rain, current.pressure_with_history_fallback
                ),
            ),
            (
                "Comfort",
                format!("{}  index {}", current.comfort_label, current.comfort_index),
            ),
            (
                "Wind",
                format!(
                    "{} {}  {}",
                    current.wind, current.wind_speed, current.wind_degree
                ),
            ),
            ("Sunrise", current.sunrise.clone()),
            ("Sunset", current.sunset.clone()),
            ("Published", current.publish_time.clone()),
        ]
    } else {
        vec![("Status", "No weather snapshot loaded.".to_string())]
    }
}

fn temperature_summary(current: &CurrentWeatherView) -> String {
    let mut parts = vec![format!("current {}", current.temperature)];
    if current.high_temperature.is_some() || current.low_temperature.is_some() {
        parts.push(format!(
            "high {}",
            current.high_temperature.as_deref().unwrap_or("-")
        ));
        parts.push(format!(
            "low {}",
            current.low_temperature.as_deref().unwrap_or("-")
        ));
    }
    parts.join("  ")
}

fn draw_forecast(
    frame: &mut Frame<'_>,
    area: Rect,
    forecast: Option<&ForecastView>,
    focus: PanelFocus,
    forecast_scroll: usize,
) {
    let visible_rows = area.height.saturating_sub(3) as usize;
    let rows = forecast
        .map(|forecast| {
            let start = forecast_scroll.min(forecast.days.len().saturating_sub(1));
            forecast
                .days
                .iter()
                .skip(start)
                .take(visible_rows.max(1))
                .map(|day| {
                    Row::new(vec![
                        day.date.clone(),
                        day.day_info.clone(),
                        day.night_info.clone(),
                        format!("{}/{}", day.day_temperature, day.night_temperature),
                        day.wind.clone(),
                        day.precipitation.clone(),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(vec![
        "Date", "Day", "Night", "Temp", "Wind", "Rain",
    ]))
    .block(focused_block("Forecast", focus, PanelFocus::Forecast));
    frame.render_widget(table, area);
}

fn draw_alert(
    frame: &mut Frame<'_>,
    area: Rect,
    alert: Option<&AlertView>,
    focus: PanelFocus,
    alert_scroll: usize,
) {
    let lines = alert
        .map(|alert| alert.lines.clone())
        .unwrap_or_else(|| vec!["No active alert. Reserved for warning details.".to_string()])
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((alert_scroll as u16, 0))
            .block(focused_block("Alert", focus, PanelFocus::Alert))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(56), Constraint::Min(24)])
        .split(area);
    draw_key_hints(frame, chunks[0], app.mode, app.focus);
    draw_logs(frame, chunks[1], app);
}

fn draw_key_hints(frame: &mut Frame<'_>, area: Rect, mode: InputMode, focus: PanelFocus) {
    let lines = match mode {
        InputMode::Normal => {
            let second = match focus {
                PanelFocus::Stations => "j/k or Up/Down select station   Tab focus",
                PanelFocus::Forecast => "j/k Up/Down scroll forecast   PgUp/PgDn page",
                PanelFocus::Alert => "j/k or Up/Down scroll alert   Tab focus",
                PanelFocus::Current => "Tab focus   r refresh",
            };
            vec![
                Line::from("q quit   r refresh   m manage   ? about"),
                Line::from(second),
            ]
        }
        InputMode::Manage => vec![
            Line::from("Esc back   j/k select   Space toggle"),
            Line::from("d delete   a add   s browse   M move   h hide-toggle"),
        ],
        InputMode::ManageMove => vec![
            Line::from("j/k move row   Enter confirm"),
            Line::from("Esc cancel"),
        ],
        InputMode::ManageAddSearch => vec![
            Line::from("Enter search   Up/Down select   Ctrl+A add"),
            Line::from("Esc back   type to edit query"),
        ],
        InputMode::ManageBrowseSearch => vec![
            Line::from("Enter search   Up/Down select   Ctrl+A preview"),
            Line::from("Esc back   type to edit query"),
        ],
        InputMode::About => vec![
            Line::from("r refresh status"),
            Line::from("any other key close"),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Keys"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let visible_lines = area.height.saturating_sub(2) as usize;
    let lines = app
        .logs
        .iter()
        .rev()
        .take(visible_lines)
        .map(|message| Line::from(message.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Log"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_manage(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let title = if app.move_draft.is_some() {
        "Manage Stations [moving]"
    } else {
        "Manage Stations"
    };
    let stations = app
        .move_draft
        .as_ref()
        .map(|draft| draft.stations.as_slice())
        .unwrap_or(&app.config.stations);
    let items = stations
        .iter()
        .enumerate()
        .map(|(index, station)| {
            let marker = if station.enabled { "[x]" } else { "[ ]" };
            let hidden = app.hidden_stations.contains(&station.name);
            let hide_tag = if hidden { " (hidden)" } else { "" };
            let moving = app
                .move_draft
                .as_ref()
                .is_some_and(|draft| draft.selected == index);
            let prefix = if moving { ">>" } else { "  " };
            ListItem::new(format!(
                "{prefix}{:>2}. {marker} {}{hide_tag}",
                index + 1,
                station.name
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.manage_selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow)),
        area,
        &mut state,
    );
}

fn draw_search(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let mode_label = match app.mode {
        InputMode::ManageAddSearch => "[Add]",
        InputMode::ManageBrowseSearch => "[Browse]",
        _ => "",
    };
    let loading_label = if app.search.status == SearchStatus::Loading {
        " loading…"
    } else {
        ""
    };

    let show_preview = app.mode == InputMode::ManageBrowseSearch && app.preview_snapshot.is_some();
    let constraints = if show_preview {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(8),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    frame.render_widget(
        Paragraph::new(app.search.query.clone()).block(
            Block::default().borders(Borders::ALL).title(format!(
                "Search {mode_label} (page {}{loading_label})",
                app.search.next_offset / SEARCH_PAGE_SIZE
            )),
        ),
        chunks[0],
    );

    let items = app
        .search
        .results
        .iter()
        .map(|station| ListItem::new(station.name.clone()))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.search.selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Results"))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Green)),
        chunks[1],
        &mut state,
    );

    if show_preview {
        if let Some(snapshot) = app.preview_snapshot.as_ref() {
            let weather = WeatherView::new(snapshot);
            draw_current(
                frame,
                chunks[2],
                weather.current.as_ref(),
                PanelFocus::Alert,
            );
        }
    } else {
        let hint = match (&app.search.status, app.mode) {
            (SearchStatus::Failed(error), _) => error.as_str(),
            (_, InputMode::ManageAddSearch) => {
                "Enter search | Up/Down select | Ctrl+A add | Esc back"
            }
            (_, InputMode::ManageBrowseSearch) => {
                "Enter search | Up/Down select | Ctrl+A preview | Esc back"
            }
            _ => "",
        };
        frame.render_widget(Paragraph::new(hint), chunks[2]);
    }
}

fn draw_about(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("About")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = about_sections(app.about_status.as_ref());
    let mut constraints = Vec::new();
    for section in &sections {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(section.rows.len() as u16));
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut chunk_index = 0;
    for section in sections {
        frame.render_widget(
            Paragraph::new(section.title).style(Style::default().fg(section.color)),
            chunks[chunk_index],
        );
        chunk_index += 1;
        let rows = section.rows.into_iter().map(|row| {
            Row::new(vec![
                Cell::from(row.label).style(Style::default().fg(section.color)),
                Cell::from(row.value),
            ])
        });
        frame.render_widget(
            Table::new(rows, [Constraint::Length(16), Constraint::Min(10)]),
            chunks[chunk_index],
        );
        chunk_index += 2;
    }
    frame.render_widget(
        Paragraph::new("r refresh | any other key close"),
        chunks[chunk_index],
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AboutRow {
    label: &'static str,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AboutSection {
    title: &'static str,
    color: Color,
    rows: Vec<AboutRow>,
}

fn about_sections(status: Option<&EngineStatus>) -> Vec<AboutSection> {
    let mut sections = vec![AboutSection {
        title: "TUI",
        color: Color::Yellow,
        rows: vec![
            AboutRow {
                label: "Version",
                value: env!("CARGO_PKG_VERSION").to_string(),
            },
            AboutRow {
                label: "Build",
                value: env!("BUILD_VERSION").to_string(),
            },
        ],
    }];

    if let Some(status) = status {
        sections.push(AboutSection {
            title: "Engine",
            color: Color::Cyan,
            rows: vec![
                AboutRow {
                    label: "Version",
                    value: status.engine_version.clone(),
                },
                AboutRow {
                    label: "Build",
                    value: status.build_version.clone(),
                },
                AboutRow {
                    label: "Schema",
                    value: status.schema_version.clone(),
                },
            ],
        });
        sections.push(AboutSection {
            title: "Runtime",
            color: Color::Green,
            rows: vec![
                AboutRow {
                    label: "Mode",
                    value: status.mode.clone(),
                },
                AboutRow {
                    label: "Ready",
                    value: status.ready.to_string(),
                },
                AboutRow {
                    label: "RPC endpoint",
                    value: status.rpc_endpoint.clone(),
                },
                AboutRow {
                    label: "PUB endpoint",
                    value: status.pub_endpoint.clone(),
                },
                AboutRow {
                    label: "Config",
                    value: status.config_path.clone(),
                },
                AboutRow {
                    label: "Last cfg error",
                    value: status
                        .last_config_error
                        .clone()
                        .unwrap_or_else(|| "none".to_string()),
                },
            ],
        });
    } else {
        sections.push(AboutSection {
            title: "Engine",
            color: Color::Cyan,
            rows: vec![AboutRow {
                label: "Status",
                value: "unavailable".to_string(),
            }],
        });
    }

    sections
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use weather_schema::{
        ForecastDay, ForecastReport, ObservedWeather, PassedWeatherChart, RadarInfo,
        TemperatureChart, WeatherAlert, WeatherSnapshot,
    };

    use super::*;

    fn status() -> EngineStatus {
        EngineStatus {
            ready: true,
            mode: "foreground".to_string(),
            rpc_endpoint: "tcp://127.0.0.1:44445".to_string(),
            pub_endpoint: "tcp://127.0.0.1:44444".to_string(),
            config_path: "/tmp/weather/config/weather.toml".to_string(),
            last_config_error: None,
            message: None,
            engine_version: "0.1.0".to_string(),
            schema_version: "v1".to_string(),
            build_version: "dev".to_string(),
            instance_id: "test-instance".to_string(),
        }
    }

    #[test]
    fn about_sections_group_table_rows() {
        let sections = about_sections(Some(&status()));

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].title, "TUI");
        assert_eq!(sections[0].rows[0].label, "Version");
        assert_eq!(sections[1].title, "Engine");
        assert_eq!(sections[1].rows[2].label, "Schema");
        assert_eq!(sections[2].title, "Runtime");
        assert_eq!(sections[2].rows[4].label, "Config");
        assert_eq!(sections[2].rows[5].value, "none");
    }

    #[test]
    fn about_sections_show_unavailable_status() {
        let sections = about_sections(None);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].title, "Engine");
        assert_eq!(sections[1].rows[0].label, "Status");
        assert_eq!(sections[1].rows[0].value, "unavailable");
    }

    #[test]
    fn main_layout_reserves_two_content_lines_for_footer() {
        let chunks = main_layout(Rect::new(0, 0, 100, 24));

        assert_eq!(chunks[0].height, 3);
        assert_eq!(chunks[2].height, 4);
    }

    #[test]
    fn body_layout_compresses_forecast_to_two_lines_first() {
        let chunks = right_panel_layout(Rect::new(0, 0, 80, 13), true);

        assert_eq!(chunks[0].height, 8);
        assert_eq!(chunks[1].height, 2);
        assert_eq!(chunks[2].height, 3);
    }

    #[test]
    fn current_rows_are_label_value_pairs() {
        let weather = WeatherView::new(&WeatherSnapshot {
            real: Some(ObservedWeather {
                info: Some("Sunny".to_string()),
                temperature: Some(12.0),
                feel_temperature: Some(10.0),
                humidity: Some(55.0),
                rain: Some(0.0),
                wind_direct: Some("North".to_string()),
                wind_power: Some("Light".to_string()),
                wind_speed: Some(3.0),
                sunrise: Some("06:00".to_string()),
                sunset: Some("18:00".to_string()),
                publish_time: Some("12:00".to_string()),
                air_pressure: Some(1001.0),
                comfort_index: Some("3".to_string()),
                comfort_label: Some("Comfortable".to_string()),
                wind_degree: Some(10.0),
                ..Default::default()
            }),
            predict: Some(ForecastReport {
                publish_time: Some("11:00".to_string()),
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    day_temperature: Some("18".to_string()),
                    night_temperature: Some("8".to_string()),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        });
        let rows = current_rows(weather.current.as_ref());

        assert_eq!(rows[0].0, "Weather");
        assert_eq!(rows[0].1, "Sunny");
        assert_eq!(rows[1].0, "Temperature");
        assert!(rows[1].1.contains("12"));
        assert!(rows[1].1.contains("18"));
        assert!(rows[1].1.contains('8'));
        assert_eq!(rows[2], ("Feels like", "10℃".to_string()));
        assert!(rows[3].1.contains("1001hPa"));
        assert!(rows[4].1.contains("Comfortable"));
        assert!(rows[5].1.contains("Light"));
        assert_eq!(rows[6], ("Sunrise", "06:00".to_string()));
    }

    #[test]
    fn current_rows_keep_temperature_and_pressure_fallbacks() {
        let weather = WeatherView::new(&WeatherSnapshot {
            real: Some(ObservedWeather {
                info: Some("Cloudy".to_string()),
                temperature: Some(27.0),
                ..Default::default()
            }),
            predict: Some(ForecastReport {
                days: vec![ForecastDay {
                    date: "2026-06-29".to_string(),
                    night_temperature: Some("21".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            tempchart: vec![TemperatureChart {
                date: Some("2026/06/29".to_string()),
                max_temperature: Some(27.1),
                min_temperature: Some(21.0),
                ..Default::default()
            }],
            passedchart: vec![PassedWeatherChart {
                pressure: Some(998.0),
                ..Default::default()
            }],
            ..Default::default()
        });
        let rows = current_rows(weather.current.as_ref());

        assert!(rows[1].1.contains("high 27.1℃"));
        assert!(rows[1].1.contains("low 21℃"));
        assert!(rows[3].1.contains("pressure 998hPa"));
    }

    #[test]
    fn alert_lines_only_include_alert_details() {
        let weather = WeatherView::new(&WeatherSnapshot {
            real: Some(ObservedWeather {
                alert: Some(WeatherAlert {
                    alert: Some("雷电黄色预警".to_string()),
                    signal_level: Some("黄色".to_string()),
                    issue_content: Some("雷阵雨天气".to_string()),
                    prevention: Some("注意防范".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            radar: Some(RadarInfo {
                title: Some("华北".to_string()),
                image_url: Some("https://www.nmc.cn/radar.png".to_string()),
                ..Default::default()
            }),
            passedchart: vec![PassedWeatherChart {
                rain_1h: Some(0.0),
                temperature: Some(27.2),
                wind_speed: Some(2.9),
                ..Default::default()
            }],
            ..Default::default()
        });

        let joined = weather.alert.unwrap().lines.join("\n");
        assert!(joined.contains("雷电黄色预警"));
        assert!(joined.contains("雷阵雨天气"));
        assert!(!joined.contains("Radar"));
        assert!(!joined.contains("Recent rain"));
    }
}
