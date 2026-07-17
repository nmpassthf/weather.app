import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { assets } from "./assets";
import type {
  AppConfig,
  BootstrapPayload,
  DailyTemperaturePoint,
  EngineStatus,
  GuiConfig,
  StationRef,
  TemperatureHistoryResponse,
  WeatherSnapshot,
} from "./types";

const demoParameters = new URLSearchParams(window.location.search);
const demoMode = import.meta.env.DEV && demoParameters.has("demo");
const demoLatency = Math.min(Math.max(Number(demoParameters.get("latency")) || 0, 0), 2_000);
const demoRefreshFailure = demoParameters.has("failRefresh");
const demoShortHistory = demoParameters.has("shortHistory");

const demoEngine: EngineStatus = {
  ready: true,
  mode: "foreground",
  rpc_endpoint: "tcp://127.0.0.1:41001",
  pub_endpoint: "tcp://127.0.0.1:41002",
  config_path: "~/.weather/config/weather.toml",
  engine_version: "0.1.0",
  schema_version: "v1",
  build_version: "0.1.0",
  instance_id: "demo-instance",
  lifecycle_state: 2,
};

let demoConfig: AppConfig = {
  config_version: 2,
  stations: [
    { name: "北京-北京市", enabled: true },
    { name: "上海-上海市", enabled: true },
    { name: "海南-海南省-三亚", enabled: true },
    { name: "杭州-浙江省-杭州市", enabled: false },
  ],
};

let demoGuiConfig: GuiConfig = {
  configVersion: 1,
  debug: false,
  configPath: "~/.weather/config/weather-gui.toml",
};

const demoStations: StationRef[] = [
  { province: "北京市", city: "北京", name: "北京-北京市", unified_uuid: "demo-beijing" },
  { province: "北京市", city: "朝阳", name: "北京-北京市-朝阳", unified_uuid: "demo-beijing-chaoyang" },
  { province: "上海市", city: "上海", name: "上海-上海市", unified_uuid: "demo-shanghai" },
  { province: "海南省", city: "三亚", name: "海南-海南省-三亚", unified_uuid: "demo-sanya" },
  { province: "浙江省", city: "杭州", name: "杭州-浙江省-杭州市", unified_uuid: "demo-hangzhou" },
];

const demoTemperatureHistory: DailyTemperaturePoint[] = [
  ...Array.from({ length: 30 }, (_, index) => {
    const offset = index - 30;
    const date = new Date(Date.UTC(2026, 6, 17 + offset)).toISOString().slice(0, 10);
    return {
      date,
      max_temperature: 31 + Math.sin(index / 2.7) * 3,
      min_temperature: 23 + Math.sin(index / 3.1) * 2,
      forecast: false,
    };
  }),
  ...Array.from({ length: 10 }, (_, index) => ({
    date: new Date(Date.UTC(2026, 6, 17 + index)).toISOString().slice(0, 10),
    max_temperature: 32 + Math.sin(index / 1.7) * 3,
    min_temperature: 24 + Math.sin(index / 2.1) * 2,
    forecast: true,
  })),
];

function demoStationMatches(station: StationRef, query: string): boolean {
  const compact = (value: string): string => value.toLocaleLowerCase().replace(/[^\p{L}\p{N}]+/gu, "");
  if (!query.trim()) return true;
  const tokens = query.split(/[^\p{L}\p{N}]+/u).map(compact).filter(Boolean);
  if (!tokens.length) return false;
  const shortProvince = station.province.replace(/(?:特别行政区|自治区|市|省)$/u, "");
  const values = [
    station.name,
    station.province,
    shortProvince,
    station.city,
    `${station.province}${station.city}`,
    `${shortProvince}${station.city}`,
  ].map(compact);
  return tokens.every((token) => values.some((value) => value.includes(token)));
}

function demoWeather(name = "北京-北京市"): WeatherSnapshot {
  const isShanghai = name.includes("上海");
  const isSanya = name.includes("三亚");
  return {
    station: demoStations.find((station) => station.name === name) ?? {
      province: "北京市", city: "北京", name, unified_uuid: "demo-station",
    },
    real: {
      publish_time: "2026-07-17 11:30",
      info: isSanya ? "-" : isShanghai ? "中雨" : "晴间多云",
      temperature: isShanghai ? 27.4 : 31.6,
      feel_temperature: isShanghai ? 30.1 : 33.2,
      humidity: isShanghai ? 82 : 51,
      rain: isShanghai ? 4.2 : 0,
      wind_direct: "东南风",
      wind_power: "3级",
      wind_speed: 3.4,
      sunrise: "05:01",
      sunset: "19:39",
      air_pressure: 1002,
      comfort_label: "较舒适",
      comfort_index: "体感温暖",
      alerts: isSanya ? [
        {
          alert: "三亚市雷雨大风黄色预警",
          province: "海南省",
          city: "三亚",
          inherited: false,
          signal_level: "黄色",
          issue_content: "三亚部分地区将出现雷雨大风天气。",
          prevention: "减少户外活动并远离临时搭建物。",
        },
        {
          alert: "海南省高温橙色预警",
          province: "海南省",
          city: "海南省",
          inherited: true,
          signal_level: "橙色",
          issue_content: "海南省部分地区最高气温将达到 37℃ 以上。",
          prevention: "注意防暑降温。",
        },
      ] : isShanghai ? [{
        alert: "暴雨蓝色预警",
        province: "上海市",
        city: "上海",
        inherited: false,
        signal_level: "蓝色",
        issue_content: "预计未来六小时部分地区有短时强降水。",
        prevention: "请注意道路积水与出行安全。",
      }] : [],
    },
    predict: {
      publish_time: "2026-07-17 08:00",
      days: [
        ["07-17", isSanya ? "多云" : "晴间多云", "多云", "33", "24"],
        ["07-18", "多云", "阵雨", "32", "23"],
        ["07-19", "小雨", "小雨", "29", "22"],
        ["07-20", "阴", "多云", "30", "23"],
        ["07-21", "晴", "晴", "34", "25"],
        ["07-22", "多云", "多云", "33", "25"],
      ].map(([date, day, night, high, low]) => ({
        date: date ?? "", day_info: day, night_info: night,
        day_temperature: high, night_temperature: low,
        day_wind_direct: "东南风", day_wind_power: "2-3级",
      })),
    },
    air: {
      publish_time: "11:00", aqi: 42, category: "优", level: "一级",
      primary_pollutant: null, pm2_5: 18, pm10: 35, no2: 21, so2: 6, co: 0.7, o3: 72,
    },
    tempchart: [],
    passedchart: Array.from({ length: 13 }, (_, index) => ({
      time: `${new Date(Date.UTC(2026, 6, 16, 23 + index)).toISOString().slice(0, 10)} ${String((23 + index) % 24).padStart(2, "0")}:00`,
      temperature: 24 + Math.sin(index / 3) * 4 + index * 0.35,
      humidity: 70 - index,
    })),
    climate: {
      period: "1991—2020 常年值",
      month: Array.from({ length: 6 }, (_, index) => ({
        month: index + 4,
        average_max_temperature: 21 + index * 3,
        average_min_temperature: 10 + index * 2.4,
        precipitation: 35 + index * 12,
      })),
    },
    radar: { title: "华北区域雷达（演示）", image_resource_id: "demo-radar" },
    stale: false,
  };
}

export async function invokeCommand<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (!demoMode) return invoke<T>(command, args);
  if (demoLatency > 0 && (command === "get_weather" || command === "search_stations")) {
    await new Promise((resolve) => window.setTimeout(resolve, demoLatency));
  }
  const stationName = String(args.stationName ?? "北京-北京市");
  if (command === "get_weather" && args.refresh === true && demoRefreshFailure) {
    throw new Error("模拟上游天气更新失败");
  }
  const result = (() => {
    switch (command) {
      case "bootstrap": {
        const cached = { ...demoWeather(), radar: null, stale: true };
        return { config: demoConfig, status: demoEngine, initialWeather: cached, cachedWeather: [cached] } satisfies BootstrapPayload;
      }
      case "get_weather": return demoWeather(stationName);
      case "get_temperature_history": {
        const beforeDate = typeof args.beforeDate === "string" && args.beforeDate
          ? args.beforeDate
          : null;
        const pageSize = Math.min(31, Math.max(1, Number(args.pageSize) || 7));
        const allHistory = demoTemperatureHistory.filter((point) => !point.forecast);
        const historyNewestFirst = (demoShortHistory ? allHistory.slice(-1) : allHistory).reverse();
        const eligibleHistory = beforeDate
          ? historyNewestFirst.filter((point) => point.date < beforeDate)
          : historyNewestFirst;
        const historyNewestPage = eligibleHistory.slice(0, pageSize);
        const historyPage = historyNewestPage.slice().reverse();
        const forecast = beforeDate === null
          ? demoTemperatureHistory.filter((point) => point.forecast)
          : [];
        const hasMoreHistory = historyNewestPage.length < eligibleHistory.length;
        return {
          points: [...historyPage, ...forecast],
          next_before_date: hasMoreHistory ? historyPage[0]?.date ?? null : null,
          has_more_history: hasMoreHistory,
        } satisfies TemperatureHistoryResponse;
      }
      case "search_stations": return demoStations.filter((station) => demoStationMatches(station, String(args.query ?? "")));
      case "update_stations":
        demoConfig = { ...demoConfig, stations: (args.stations as AppConfig["stations"]) ?? [] };
        return demoConfig;
      case "engine_status": return demoEngine;
      case "get_config_text": return "config_version = 2\n\n[[stations]]\nname = \"北京-北京市\"\nenabled = true\n";
      case "get_gui_config": return demoGuiConfig;
      case "set_gui_debug":
        demoGuiConfig = { ...demoGuiConfig, debug: args.debug === true };
        return demoGuiConfig;
      case "open_gui_devtools": return undefined;
      case "restart_gui": return undefined;
      case "restart_engine": return "引擎已接受重启请求";
      case "stop_engine": return "引擎已接受停止请求";
      default: throw new Error(`demo command not implemented: ${command}`);
    }
  })();
  return structuredClone(result) as T;
}

export async function invokeBinaryCommand(command: string, args: Record<string, unknown> = {}): Promise<ArrayBuffer> {
  if (demoMode) {
    if (command !== "get_resource_bytes" || args.resourceId !== "demo-radar") {
      throw new Error(`demo binary command not implemented: ${command}`);
    }
    const response = await fetch(assets.radarDemo);
    if (!response.ok) throw new Error(`demo radar asset returned HTTP ${response.status}`);
    return response.arrayBuffer();
  }
  return invoke<ArrayBuffer>(command, args);
}

export async function listenEvent<T>(event: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
  if (!demoMode) return listen<T>(event, handler);
  return () => undefined;
}
