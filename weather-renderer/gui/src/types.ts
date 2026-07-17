export interface StationConfig {
  name: string;
  enabled: boolean;
}

export interface AppConfig {
  stations: StationConfig[];
  config_version: number;
}

export interface StationRef {
  province: string;
  city: string;
  name: string;
  unified_uuid: string;
}

export interface EngineStatus {
  ready: boolean;
  mode: string;
  rpc_endpoint: string;
  pub_endpoint: string;
  config_path: string;
  last_config_error?: string | null;
  message?: string | null;
  engine_version: string;
  schema_version: string;
  build_version: string;
  instance_id: string;
  lifecycle_state: number;
}

export interface WeatherAlert {
  alert?: string | null;
  province?: string | null;
  city?: string | null;
  issue_content?: string | null;
  prevention?: string | null;
  signal_level?: string | null;
  icon_resource_id?: string | null;
  inherited: boolean;
}

export interface ObservedWeather {
  publish_time?: string | null;
  info?: string | null;
  temperature?: number | null;
  feel_temperature?: number | null;
  humidity?: number | null;
  rain?: number | null;
  wind_direct?: string | null;
  wind_power?: string | null;
  wind_speed?: number | null;
  sunrise?: string | null;
  sunset?: string | null;
  alerts: WeatherAlert[];
  temperature_diff?: number | null;
  air_pressure?: number | null;
  comfort_index?: string | null;
  comfort_label?: string | null;
  weather_icon?: string | null;
  wind_degree?: number | null;
}

export interface ForecastDay {
  date: string;
  day_info?: string | null;
  night_info?: string | null;
  day_temperature?: string | null;
  night_temperature?: string | null;
  wind_direct?: string | null;
  wind_power?: string | null;
  precipitation?: number | null;
  day_weather_icon?: string | null;
  night_weather_icon?: string | null;
  day_wind_direct?: string | null;
  day_wind_power?: string | null;
  night_wind_direct?: string | null;
  night_wind_power?: string | null;
}

export interface AirQuality {
  publish_time?: string | null;
  aqi?: number | null;
  level?: string | null;
  category?: string | null;
  primary_pollutant?: string | null;
  pm2_5?: number | null;
  pm10?: number | null;
  no2?: number | null;
  so2?: number | null;
  co?: number | null;
  o3?: number | null;
}

export interface PassedWeatherChart {
  time?: string | null;
  rain_1h?: number | null;
  rain_6h?: number | null;
  rain_12h?: number | null;
  rain_24h?: number | null;
  temperature?: number | null;
  humidity?: number | null;
  pressure?: number | null;
  wind_speed?: number | null;
}

export interface DailyTemperaturePoint {
  date: string;
  max_temperature?: number | null;
  min_temperature?: number | null;
  forecast: boolean;
}

export interface TemperatureHistoryResponse {
  points: DailyTemperaturePoint[];
  next_before_date?: string | null;
  has_more_history: boolean;
}

export interface ClimateMonth {
  month?: number | null;
  average_max_temperature?: number | null;
  average_min_temperature?: number | null;
  precipitation?: number | null;
}

export interface WeatherSnapshot {
  station?: StationRef | null;
  real?: ObservedWeather | null;
  predict?: { publish_time?: string | null; days: ForecastDay[] } | null;
  air?: AirQuality | null;
  tempchart: unknown[];
  passedchart: PassedWeatherChart[];
  climate?: { period?: string | null; month: ClimateMonth[] } | null;
  radar?: {
    title?: string | null;
    image_resource_id?: string | null;
  } | null;
  stale: boolean;
}

export interface BootstrapPayload {
  config: AppConfig;
  status: EngineStatus;
  initialWeather?: WeatherSnapshot | null;
  cachedWeather: WeatherSnapshot[];
}

export interface GuiConfig {
  configVersion: number;
  debug: boolean;
  configPath: string;
}

export type GuiEngineEvent =
  | { type: "weather"; snapshot: WeatherSnapshot }
  | { type: "status"; status: EngineStatus }
  | { type: "fetch"; topic: string; event: Record<string, unknown> }
  | { type: "refresh"; topic: string; event: Record<string, unknown> }
  | { type: "log"; level: string; message: string };
