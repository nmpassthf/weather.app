use tokio::{sync::mpsc, task::JoinSet};
use weather_schema::*;

use crate::{
    client::{EngineClient, require_config},
    pagination::PageCursor,
};

use super::state::SearchPage;

const SEARCH_PAGE_SIZE: u32 = 12;

pub(super) enum Effect {
    CancelSearch,
    Search {
        token: u64,
        query: String,
    },
    LoadWeather {
        token: u64,
        name: String,
        unified_uuid: Option<String>,
        refresh: bool,
    },
    Preview {
        token: u64,
        station: StationRef,
    },
    UpdateConfig {
        token: u64,
        config: Box<AppConfig>,
    },
    LoadAbout,
}

pub(super) enum EffectResult {
    SearchPage(SearchPage),
    SearchFailed {
        token: u64,
        message: String,
    },
    Weather {
        token: u64,
        name: String,
        result: Result<WeatherSnapshot, String>,
    },
    Preview {
        token: u64,
        station_name: String,
        result: Result<WeatherSnapshot, String>,
    },
    Config {
        token: u64,
        result: Result<AppConfig, String>,
    },
    About(Result<EngineStatus, String>),
    TaskFailed(String),
}

pub(super) struct EffectRunner {
    client: EngineClient,
    sender: mpsc::Sender<EffectResult>,
    tasks: JoinSet<()>,
    search: Option<tokio::task::AbortHandle>,
}

impl EffectRunner {
    pub fn new(client: EngineClient) -> (Self, mpsc::Receiver<EffectResult>) {
        let (sender, receiver) = mpsc::channel(64);
        (
            Self {
                client,
                sender,
                tasks: JoinSet::new(),
                search: None,
            },
            receiver,
        )
    }

    pub fn dispatch(&mut self, effect: Effect) {
        self.reap_completed();
        match effect {
            Effect::CancelSearch => self.cancel_search(),
            Effect::Search { token, query } => {
                self.cancel_search();
                let client = self.client.clone();
                let sender = self.sender.clone();
                self.search = Some(self.tasks.spawn(async move {
                    run_search(client, sender, token, query).await;
                }));
            }
            Effect::LoadWeather {
                token,
                name,
                unified_uuid,
                refresh,
            } => {
                let client = self.client.clone();
                let sender = self.sender.clone();
                self.tasks.spawn(async move {
                    let result = load_weather(&client, &name, unified_uuid, refresh)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = sender
                        .send(EffectResult::Weather {
                            token,
                            name,
                            result,
                        })
                        .await;
                });
            }
            Effect::Preview { token, station } => {
                let client = self.client.clone();
                let sender = self.sender.clone();
                self.tasks.spawn(async move {
                    let station_name = station.name.clone();
                    let uuid =
                        (!station.unified_uuid.is_empty()).then_some(station.unified_uuid.clone());
                    let result = load_weather(&client, &station.name, uuid, false)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = sender
                        .send(EffectResult::Preview {
                            token,
                            station_name,
                            result,
                        })
                        .await;
                });
            }
            Effect::UpdateConfig { token, config } => {
                let client = self.client.clone();
                let sender = self.sender.clone();
                self.tasks.spawn(async move {
                    let result = client
                        .update_config(*config)
                        .await
                        .and_then(|response| require_config(response.config, "update-config"))
                        .map_err(|error| format!("{error:#}"));
                    let _ = sender.send(EffectResult::Config { token, result }).await;
                });
            }
            Effect::LoadAbout => {
                let client = self.client.clone();
                let sender = self.sender.clone();
                self.tasks.spawn(async move {
                    let result = client.status().await.map_err(|error| format!("{error:#}"));
                    let _ = sender.send(EffectResult::About(result)).await;
                });
            }
        }
    }

    pub fn reap_completed(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                let _ = self.sender.try_send(EffectResult::TaskFailed(format!(
                    "TUI effect task failed: {error}"
                )));
            }
        }
        if self.search.as_ref().is_some_and(|task| task.is_finished()) {
            self.search = None;
        }
    }

    pub async fn shutdown(mut self) {
        self.cancel_search();
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }

    fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            search.abort();
        }
    }
}

async fn run_search(
    client: EngineClient,
    sender: mpsc::Sender<EffectResult>,
    token: u64,
    query: String,
) {
    let mut cursor = PageCursor::default();
    loop {
        let (requested_offset, page_size) = match cursor.request(SEARCH_PAGE_SIZE) {
            Ok(request) => request,
            Err(error) => {
                send_search_error(&sender, token, error).await;
                return;
            }
        };
        let response = client
            .request::<FuzzyMatchStationsRequest, FuzzyMatchStationsResponse>(
                RpcKind::FuzzyMatchStations,
                FuzzyMatchStationsRequest {
                    query: query.clone(),
                    province: None,
                    page_offset: requested_offset,
                    page_size,
                },
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                send_search_error(&sender, token, error).await;
                return;
            }
        };
        let can_continue = match cursor.advance(response.has_more, response.next_offset) {
            Ok(can_continue) => can_continue,
            Err(error) => {
                send_search_error(&sender, token, error).await;
                return;
            }
        };
        if sender
            .send(EffectResult::SearchPage(SearchPage {
                token,
                requested_offset,
                stations: response.stations,
                has_more: response.has_more,
                next_offset: response.next_offset,
            }))
            .await
            .is_err()
        {
            return;
        }
        if !can_continue {
            return;
        }
    }
}

async fn send_search_error(
    sender: &mpsc::Sender<EffectResult>,
    token: u64,
    error: impl std::fmt::Display,
) {
    let _ = sender
        .send(EffectResult::SearchFailed {
            token,
            message: error.to_string(),
        })
        .await;
}

async fn load_weather(
    client: &EngineClient,
    name: &str,
    unified_uuid: Option<String>,
    refresh: bool,
) -> anyhow::Result<WeatherSnapshot> {
    let unified_uuid = match unified_uuid {
        Some(uuid) if !uuid.is_empty() => uuid,
        _ => client.resolve_station_uuid(name).await?.unified_uuid,
    };
    client
        .request::<GetWeatherRequest, WeatherSnapshot>(
            RpcKind::GetWeather,
            GetWeatherRequest {
                unified_uuid,
                refresh,
                include_debug: false,
            },
        )
        .await
}
