import {
  assets,
  usableWeatherDescription,
  weatherAsset,
  weatherAtmosphere,
} from "./assets";
import { invokeBinaryCommand, invokeCommand as invoke, listenEvent as listen } from "./bridge";
import { calendarDateIso, multiDayDateLabel, multiDayDateTimeLabel } from "./date";
import {
  recentTemperatureHistoryRows,
  savitzkyGolayTemperaturePlotSamples,
  temperatureHistoryTickIndices,
} from "./history";
import { uiIcon, type UiIconName } from "./icons";
import {
  animateApplicationExit,
  animateBootState,
  animateChartPaths,
  animateDialogIn,
  animateDialogOut,
  animateEntrance,
  animateLayoutFrom,
  animateOverlayIn,
  animateOverlayOut,
  animateStateChange,
  captureLayout,
  motionDuration,
  motionEasing,
  playMotion,
  revealApplication,
  shouldAnimateInteraction,
  shouldReduceMotion,
  waitForMotion,
  type LayoutSnapshot,
} from "./motion";
import type {
  AppConfig,
  BootstrapPayload,
  DailyTemperaturePoint,
  EngineStatus,
  ForecastDay,
  GuiConfig,
  GuiEngineEvent,
  StationConfig,
  StationRef,
  TemperatureHistoryResponse,
  WeatherSnapshot,
} from "./types";
import "./styles.css";

const element = <T extends HTMLElement>(id: string): T => {
  const value = document.getElementById(id);
  if (!value) throw new Error(`missing #${id}`);
  return value as T;
};

const appElement = element<HTMLDivElement>("app");
const boot = element<HTMLDivElement>("boot");
const bootSpinner = element<HTMLDivElement>("boot-spinner");
const bootMessage = element<HTMLParagraphElement>("boot-message");
const bootRetry = element<HTMLButtonElement>("boot-retry");
const stationList = element<HTMLElement>("station-list");
const manageList = element<HTMLDivElement>("manage-list");
const weatherContent = element<HTMLDivElement>("weather-content");
const emptyState = element<HTMLElement>("empty-state");
const searchDialog = element<HTMLDialogElement>("search-dialog");
const manageDialog = element<HTMLDialogElement>("manage-dialog");
const aboutDialog = element<HTMLDialogElement>("about-dialog");
const runtimePanel = element<HTMLElement>("runtime-panel");
const runtimeToggle = element<HTMLButtonElement>("runtime-toggle");
const imageViewer = element<HTMLDivElement>("image-viewer");
const imageViewerPanel = element<HTMLElement>("image-viewer-panel");
const imageViewerViewport = element<HTMLDivElement>("image-viewer-viewport");
const imageViewerImage = element<HTMLImageElement>("image-viewer-image");

let config: AppConfig = { stations: [], config_version: 0 };
let engine: EngineStatus | null = null;
let guiConfig: GuiConfig = { configVersion: 1, debug: false, configPath: "" };
let guiExitPending = false;
let snapshot: WeatherSnapshot | null = null;
let selectedStation = "";
let searchResults: StationRef[] = [];
let selectedSearchResult: StationRef | null = null;
let searchDebounceTimer: number | undefined;
let searchRequestToken = 0;
let weatherRequestToken = 0;
let weatherLoading = false;
const weatherCache = new Map<string, WeatherSnapshot>();
const stationSummaryRequests = new Map<string, Promise<void>>();
const loadedStationSummaries = new Set<string>();
const hiddenStations = new Set<string>();
const activity: Array<{ time: string; message: string; level: string }> = [];
const SEARCH_DEBOUNCE_MS = 220;
const STATION_SUMMARY_CONCURRENCY = 2;
const RESOURCE_URL_CACHE_SIZE = 10;
const climateNumberFormat = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 1 });
const resourceUrls = new Map<string, string>();
const resourceRequests = new Map<string, Promise<string>>();
let radarRenderToken = 0;
const IMAGE_VIEWER_MIN_SCALE = 0.5;
const IMAGE_VIEWER_MAX_SCALE = 5;
const IMAGE_VIEWER_SCALE_STEP = 0.25;
let imageViewerScale = 1;
let imageViewerPanX = 0;
let imageViewerPanY = 0;
let imageViewerTrigger: HTMLElement | null = null;
let imageViewerResourceId = "";
let historyInteractionCleanup: (() => void) | null = null;
let dailyTemperatureInteractionCleanup: (() => void) | null = null;
let dailyTemperatureRequestToken = 0;
let imageViewerClosing = false;
let appExitStarted = false;
let toastSequence = 0;
let dataBannerToken = 0;

element<HTMLImageElement>("brand-logo").src = assets.logo;
element<HTMLImageElement>("empty-image").src = assets.emptyStations;
element<HTMLImageElement>("preview-image").src = assets.emptyStations;

function visibleStations(): StationConfig[] {
  return config.stations.filter((station) => station.enabled && !hiddenStations.has(station.name));
}

function normalizeSelection(): void {
  const visible = visibleStations();
  if (!visible.some((station) => station.name === selectedStation)) {
    selectedStation = visible[0]?.name ?? "";
  }
}

function setConnection(state: "connecting" | "connected" | "failed", animate = true): void {
  const pill = element<HTMLSpanElement>("connection-state");
  const changed = pill.dataset.state !== state;
  pill.dataset.state = state;
  pill.lastChild!.textContent = state === "connected" ? "已连接" : state === "connecting" ? "连接中" : "连接失败";
  if (animate && changed && !appElement.hidden) animateStateChange(pill);
}

function log(message: string, level = "info"): void {
  activity.unshift({
    time: new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(new Date()),
    message,
    level,
  });
  activity.splice(40);
  renderLog(true);
}

function renderLog(animateNewest = false): void {
  const target = element<HTMLDivElement>("event-log");
  target.replaceChildren();
  if (activity.length === 0) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "暂无活动记录";
    target.append(empty);
    return;
  }
  activity.forEach((item, index) => {
    const row = document.createElement("div");
    row.className = `log-row ${item.level}`;
    const dot = document.createElement("i");
    const text = document.createElement("span");
    text.textContent = item.message;
    const time = document.createElement("time");
    time.textContent = item.time;
    row.append(dot, text, time);
    target.append(row);
    if (animateNewest && index === 0 && runtimePanel.dataset.open === "true") {
      animateEntrance(row, 0, 4);
    }
  });
}

function setRuntimePanel(open: boolean, restoreFocus = false, animate = true): void {
  if (!open && runtimePanel.contains(document.activeElement)) {
    if (restoreFocus) {
      runtimeToggle.focus();
    } else if (document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }
  }
  runtimePanel.dataset.motion = animate && !shouldReduceMotion() ? "animated" : "instant";
  runtimeToggle.dataset.motion = runtimePanel.dataset.motion;
  runtimePanel.dataset.open = String(open);
  runtimePanel.setAttribute("aria-hidden", String(!open));
  runtimePanel.inert = !open;
  runtimeToggle.setAttribute("aria-expanded", String(open));
  if (open) {
    void runtimePanel.offsetWidth;
    element<HTMLButtonElement>("runtime-close").focus();
  } else if (restoreFocus && document.activeElement !== runtimeToggle) {
    runtimeToggle.focus();
  }
  if (!animate || shouldReduceMotion()) {
    void runtimePanel.offsetWidth;
    window.requestAnimationFrame(() => {
      delete runtimePanel.dataset.motion;
      delete runtimeToggle.dataset.motion;
    });
  }
}

function imageViewerOpen(): boolean {
  return !imageViewer.hidden;
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.min(Math.max(value, minimum), maximum);
}

function svgUserPointToClient(
  svg: SVGSVGElement,
  x: number,
  y: number,
): { x: number; y: number } | null {
  const matrix = svg.getScreenCTM();
  if (!matrix) return null;
  const point = svg.createSVGPoint();
  point.x = x;
  point.y = y;
  const clientPoint = point.matrixTransform(matrix);
  return { x: clientPoint.x, y: clientPoint.y };
}

function svgClientPointToUser(
  svg: SVGSVGElement,
  x: number,
  y: number,
): { x: number; y: number } | null {
  const matrix = svg.getScreenCTM();
  if (!matrix) return null;
  const point = svg.createSVGPoint();
  point.x = x;
  point.y = y;
  try {
    const userPoint = point.matrixTransform(matrix.inverse());
    return { x: userPoint.x, y: userPoint.y };
  } catch {
    return null;
  }
}

function nearestPlotPointIndex(points: ReadonlyArray<{ x: number }>, x: number): number {
  let nearestIndex = 0;
  let nearestDistance = Number.POSITIVE_INFINITY;
  points.forEach((point, index) => {
    const distance = Math.abs(point.x - x);
    if (distance < nearestDistance) {
      nearestDistance = distance;
      nearestIndex = index;
    }
  });
  return nearestIndex;
}

function clampImageViewerPan(): void {
  const imageWidth = imageViewerImage.offsetWidth * imageViewerScale;
  const imageHeight = imageViewerImage.offsetHeight * imageViewerScale;
  const maximumX = Math.max(0, (imageWidth - imageViewerViewport.clientWidth) / 2);
  const maximumY = Math.max(0, (imageHeight - imageViewerViewport.clientHeight) / 2);
  imageViewerPanX = clamp(imageViewerPanX, -maximumX, maximumX);
  imageViewerPanY = clamp(imageViewerPanY, -maximumY, maximumY);
}

function updateImageViewerTransform(animate = true): void {
  imageViewerViewport.dataset.motion = animate && !shouldReduceMotion() ? "smooth" : "instant";
  clampImageViewerPan();
  imageViewerImage.style.transform = `translate(-50%, -50%) translate(${imageViewerPanX}px, ${imageViewerPanY}px) scale(${imageViewerScale})`;
  element<HTMLOutputElement>("image-viewer-scale").value = `${Math.round(imageViewerScale * 100)}%`;
  element<HTMLButtonElement>("image-viewer-zoom-out").disabled = imageViewerScale <= IMAGE_VIEWER_MIN_SCALE;
  element<HTMLButtonElement>("image-viewer-zoom-in").disabled = imageViewerScale >= IMAGE_VIEWER_MAX_SCALE;
  imageViewerViewport.dataset.pannable = String(imageViewerPanX !== 0
    || imageViewerPanY !== 0
    || imageViewerImage.offsetWidth * imageViewerScale > imageViewerViewport.clientWidth
    || imageViewerImage.offsetHeight * imageViewerScale > imageViewerViewport.clientHeight);
  if (!animate || shouldReduceMotion()) {
    void imageViewerImage.offsetWidth;
    window.requestAnimationFrame(() => delete imageViewerViewport.dataset.motion);
  }
}

function setImageViewerScale(scale: number, animate = true): void {
  imageViewerScale = clamp(
    Math.round(scale / IMAGE_VIEWER_SCALE_STEP) * IMAGE_VIEWER_SCALE_STEP,
    IMAGE_VIEWER_MIN_SCALE,
    IMAGE_VIEWER_MAX_SCALE,
  );
  updateImageViewerTransform(animate);
}

function resetImageViewer(animate = true): void {
  imageViewerScale = 1;
  imageViewerPanX = 0;
  imageViewerPanY = 0;
  updateImageViewerTransform(animate);
}

function constrainImageViewerPanel(): void {
  if (!imageViewerOpen()) return;
  const margin = 8;
  const maximumLeft = Math.max(margin, window.innerWidth - imageViewerPanel.offsetWidth - margin);
  const maximumTop = Math.max(margin, window.innerHeight - imageViewerPanel.offsetHeight - margin);
  const currentLeft = Number.parseFloat(imageViewerPanel.style.left) || margin;
  const currentTop = Number.parseFloat(imageViewerPanel.style.top) || margin;
  imageViewerPanel.style.left = `${clamp(currentLeft, margin, maximumLeft)}px`;
  imageViewerPanel.style.top = `${clamp(currentTop, margin, maximumTop)}px`;
}

function centerImageViewerPanel(): void {
  imageViewerPanel.style.left = `${Math.max(8, (window.innerWidth - imageViewerPanel.offsetWidth) / 2)}px`;
  imageViewerPanel.style.top = `${Math.max(8, (window.innerHeight - imageViewerPanel.offsetHeight) / 2)}px`;
}

async function closeImageViewer(restoreFocus = true, animate = true): Promise<void> {
  if (!imageViewerOpen() || imageViewerClosing) return;
  imageViewerClosing = true;
  const trigger = imageViewerTrigger;
  if (animate && !shouldReduceMotion()) {
    await animateOverlayOut(imageViewer, imageViewerPanel);
  }
  imageViewer.hidden = true;
  imageViewer.setAttribute("aria-hidden", "true");
  appElement.inert = false;
  imageViewerImage.onload = null;
  imageViewerImage.onerror = null;
  imageViewerImage.removeAttribute("src");
  imageViewerImage.style.removeProperty("transform");
  imageViewerTrigger = null;
  imageViewerResourceId = "";
  imageViewerViewport.dataset.pannable = "false";
  imageViewerClosing = false;
  if (restoreFocus && trigger?.isConnected) trigger.focus();
}

function openImageViewer(
  source: string,
  title: string,
  trigger: HTMLElement,
  resourceId = "",
  animate = true,
): void {
  if (imageViewerOpen()) void closeImageViewer(false, false);
  imageViewerTrigger = trigger;
  imageViewerResourceId = resourceId;
  element("image-viewer-title").textContent = title;
  imageViewerImage.alt = title;
  imageViewerImage.onload = () => resetImageViewer(false);
  imageViewerImage.onerror = () => {
    closeImageViewer(false);
    toast("图片预览加载失败", "error");
  };
  imageViewer.hidden = false;
  imageViewer.setAttribute("aria-hidden", "false");
  appElement.inert = true;
  imageViewerImage.src = source;
  centerImageViewerPanel();
  resetImageViewer(false);
  imageViewerPanel.focus();
  if (animate && !shouldReduceMotion()) animateOverlayIn(imageViewer, imageViewerPanel);
}

function enableImageViewer(trigger: HTMLElement, source: string, title: string, resourceId = ""): void {
  trigger.dataset.zoomable = "true";
  trigger.tabIndex = 0;
  trigger.setAttribute("role", "button");
  trigger.setAttribute("aria-label", `放大查看${title}`);
  trigger.title = `放大查看${title}`;
  const open = (event?: Event): void => openImageViewer(
    source,
    title,
    trigger,
    resourceId,
    shouldAnimateInteraction(event),
  );
  trigger.addEventListener("click", open);
  trigger.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    open(event);
  });
}

function toast(message: string, level = "info", animate = true): void {
  const region = element<HTMLDivElement>("toast-region");
  const useMotion = animate && !shouldReduceMotion();
  const previous = useMotion ? captureLayout(region, ".toast") : null;
  const node = document.createElement("div");
  node.className = `toast ${level}`;
  node.dataset.motionKey = String(++toastSequence);
  node.textContent = message;
  region.append(node);
  if (useMotion) animateLayoutFrom(region, ".toast", previous);
  window.setTimeout(() => {
    void (async () => {
      if (useMotion) {
        await waitForMotion(playMotion(node, [
          { opacity: 1, transform: "translateY(0) scale(1)" },
          { opacity: 0, transform: "translateY(5px) scale(0.985)" },
        ], {
          duration: motionDuration.fast,
          easing: motionEasing.out,
        }));
      }
      if (!node.isConnected) return;
      const beforeRemoval = useMotion ? captureLayout(region, ".toast") : null;
      node.remove();
      if (useMotion) animateLayoutFrom(region, ".toast", beforeRemoval);
    })();
  }, 3400);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function showDataUpdateFailure(message: string, animate = true): void {
  dataBannerToken += 1;
  const summary = message.replace(/\s+/g, " ").trim().slice(0, 240) || "未能取得最新天气数据";
  element("data-update-message").textContent = `天气数据更新失败：${summary}。当前显示的可能是今日缓存数据。`;
  const banner = element<HTMLDivElement>("data-update-banner");
  const wasHidden = banner.hidden;
  banner.hidden = false;
  if (!animate || shouldReduceMotion()) return;
  if (wasHidden) {
    playMotion(banner, [
      { opacity: 0, transform: "translate(-50%, -8px) scale(0.985)" },
      { opacity: 1, transform: "translate(-50%, 0) scale(1)" },
    ], {
      duration: motionDuration.standard,
      easing: motionEasing.out,
    });
  } else {
    playMotion(banner, [
      { opacity: 0.72, transform: "translate(-50%, 2px)" },
      { opacity: 1, transform: "translate(-50%, 0)" },
    ], {
      duration: motionDuration.fast,
      easing: motionEasing.out,
    });
  }
}

function clearDataUpdateFailure(animate = true): void {
  const banner = element<HTMLDivElement>("data-update-banner");
  if (banner.hidden) return;
  if (!animate || shouldReduceMotion()) {
    dataBannerToken += 1;
    banner.hidden = true;
    return;
  }
  const token = ++dataBannerToken;
  void waitForMotion(playMotion(banner, [
    { opacity: 1, transform: "translate(-50%, 0) scale(1)" },
    { opacity: 0, transform: "translate(-50%, -6px) scale(0.99)" },
  ], {
    duration: motionDuration.fast,
    easing: motionEasing.out,
  })).then(() => {
    if (token === dataBannerToken) banner.hidden = true;
  });
}

function applyGuiConfig(next: GuiConfig): void {
  guiConfig = next;
  document.documentElement.dataset.debug = String(next.debug);
  const checkbox = element<HTMLInputElement>("debug-mode");
  checkbox.checked = next.debug;
  checkbox.disabled = guiExitPending;
  const path = element<HTMLParagraphElement>("gui-config-path");
  path.textContent = `GUI 配置：${next.configPath || "—"}`;
  path.title = next.configPath;
}

async function initializeGuiConfig(): Promise<void> {
  try {
    applyGuiConfig(await invoke<GuiConfig>("get_gui_config"));
  } catch (error) {
    const message = errorMessage(error);
    element("gui-config-path").textContent = `GUI 配置读取失败：${message}`;
    log(`GUI 配置读取失败：${message}`, "error");
  }
}

async function updateGuiDebug(debug: boolean, animate = true): Promise<void> {
  const checkbox = element<HTMLInputElement>("debug-mode");
  checkbox.disabled = true;
  try {
    const updated = await invoke<GuiConfig>("set_gui_debug", { debug });
    guiExitPending = true;
    applyGuiConfig(updated);
    toast(`GUI 调试已${debug ? "启用" : "关闭"}，请手动重启 GUI`, "success", animate);
    window.setTimeout(() => {
      void invoke<void>("exit_gui").catch((error) => {
        guiExitPending = false;
        applyGuiConfig(guiConfig);
        toast(`GUI 自动关闭失败：${errorMessage(error)}`, "error", animate);
      });
    }, 350);
  } catch (error) {
    checkbox.checked = guiConfig.debug;
    checkbox.disabled = false;
    toast(`GUI 调试设置保存失败：${errorMessage(error)}`, "error", animate);
  }
}

function isDevtoolsShortcut(event: KeyboardEvent): boolean {
  const key = event.key.toLocaleLowerCase();
  return event.key === "F12"
    || (event.ctrlKey && event.shiftKey && ["i", "j", "c"].includes(key))
    || (event.metaKey && event.altKey && ["i", "j", "c"].includes(key));
}

function openGuiDevtools(): void {
  void invoke<void>("open_gui_devtools").catch((error) => {
    toast(`无法打开审查元素：${errorMessage(error)}`, "error");
  });
}

async function initialize(): Promise<void> {
  boot.setAttribute("aria-label", "天气应用正在加载");
  bootSpinner.hidden = false;
  bootRetry.hidden = true;
  bootMessage.hidden = true;
  bootMessage.textContent = "";
  setConnection("connecting");
  animateBootState(bootSpinner);
  try {
    const payload = await invoke<BootstrapPayload>("bootstrap");
    config = payload.config;
    engine = payload.status;
    weatherCache.clear();
    stationSummaryRequests.clear();
    loadedStationSummaries.clear();
    for (const cached of payload.cachedWeather) {
      const stationName = cached.station?.name;
      if (stationName) weatherCache.set(stationName, cached);
    }
    snapshot = payload.initialWeather ?? null;
    selectedStation = snapshot?.station?.name ?? visibleStations()[0]?.name ?? "";
    if (snapshot?.station?.name) weatherCache.set(snapshot.station.name, snapshot);
    normalizeSelection();
    renderAll();
    revealApplication(boot, appElement);
    setConnection("connected");
    log("天气引擎已连接");
    void loadWeather(true);
    void loadStationSummaries();
  } catch (error) {
    const message = errorMessage(error);
    bootMessage.textContent = message.startsWith("未找到命令 `") ? message : `连接失败：${message}`;
    bootMessage.hidden = false;
    bootSpinner.hidden = true;
    bootRetry.hidden = false;
    boot.setAttribute("aria-label", bootMessage.textContent);
    setConnection("failed");
    animateBootState(bootMessage, bootRetry);
  }
}

function renderAll(): void {
  normalizeSelection();
  renderStations();
  renderManageStations();
  renderEngine();
  renderWeather(snapshot, Boolean(selectedStation && !snapshot));
}

function renderStations(): void {
  stationList.replaceChildren();
  const stations = visibleStations();
  for (const station of stations) {
    const button = document.createElement("button");
    button.className = "station-item";
    button.dataset.active = String(station.name === selectedStation);
    const name = document.createElement("span");
    name.className = "station-name";
    name.textContent = station.name;
    const cached = weatherCache.get(station.name);
    const description = currentWeatherDescription(cached) ?? "—";
    const currentTemperature = value(cached?.real?.temperature, "°", 1);
    const firstDay = cached?.predict?.days[0];
    const highTemperature = sidebarForecastTemperature(firstDay?.day_temperature);
    const lowTemperature = sidebarForecastTemperature(firstDay?.night_temperature);
    const brief = document.createElement("span");
    brief.className = "station-brief";
    const current = document.createElement("span");
    current.className = "station-brief-current";
    const condition = document.createElement("span");
    condition.className = "station-condition";
    condition.textContent = description;
    const temperature = document.createElement("strong");
    temperature.className = "station-temperature";
    temperature.textContent = currentTemperature;
    current.append(condition, temperature);
    const range = document.createElement("small");
    range.className = "station-range";
    range.textContent = `最高 ${highTemperature} · 最低 ${lowTemperature}`;
    brief.append(current, range);
    button.setAttribute(
      "aria-label",
      `${station.name}，天气 ${description}，实时气温 ${currentTemperature}，最高 ${highTemperature}，最低 ${lowTemperature}`,
    );
    button.setAttribute("aria-busy", String(station.name === selectedStation && weatherLoading));
    button.append(name, brief);
    button.addEventListener("click", (event) => {
      void selectStation(station.name, shouldAnimateInteraction(event));
    });
    stationList.append(button);
  }
  const isEmpty = stations.length === 0;
  emptyState.hidden = !isEmpty;
  weatherContent.hidden = isEmpty;
}

function sidebarForecastTemperature(input: string | null | undefined): string {
  const metric = input?.trim();
  if (!metric) return "—";
  if (/[°℃]$/u.test(metric)) return metric.replace(/℃$/u, "°");
  return `${metric}°`;
}

function loadStationSummary(stationName: string): Promise<void> {
  const pending = stationSummaryRequests.get(stationName);
  if (pending) return pending;
  const request = invoke<WeatherSnapshot>("get_weather", {
    stationName,
    unifiedUuid: null,
    refresh: false,
  }).then((weather) => {
    weatherCache.set(stationName, weather);
    loadedStationSummaries.add(stationName);
    renderStations();
  }).catch((error) => {
    log(`${stationName} 天气简报加载失败：${errorMessage(error)}`, "warning");
  }).finally(() => {
    stationSummaryRequests.delete(stationName);
  });
  stationSummaryRequests.set(stationName, request);
  return request;
}

async function loadStationSummaries(): Promise<void> {
  const queue = visibleStations()
    .map((station) => station.name)
    .filter((name) => name !== selectedStation && !loadedStationSummaries.has(name));
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < queue.length) {
      const stationName = queue[next++];
      if (stationName) await loadStationSummary(stationName);
    }
  };
  await Promise.all(Array.from(
    { length: Math.min(STATION_SUMMARY_CONCURRENCY, queue.length) },
    worker,
  ));
}

async function selectStation(name: string, animate = true): Promise<void> {
  if (name === selectedStation && snapshot) return;
  selectedStation = name;
  const cached = weatherCache.get(name);
  snapshot = cached ?? null;
  if (cached) {
    renderWeather(cached);
  } else {
    renderWeather(null, true);
  }
  renderStations();
  await loadWeather(true, animate, false);
}

async function loadWeather(
  refresh: boolean,
  animate = true,
  animateContent = animate,
): Promise<void> {
  if (!selectedStation) return;
  const stationName = selectedStation;
  const requestToken = ++weatherRequestToken;
  const button = element<HTMLButtonElement>("refresh-button");
  button.disabled = animateContent;
  button.classList.toggle("spinning", animateContent);
  setWeatherLoading(true, animateContent);
  renderStations();
  try {
    const weather = await invoke<WeatherSnapshot>("get_weather", {
      stationName,
      unifiedUuid: null,
      refresh,
    });
    if (requestToken !== weatherRequestToken || selectedStation !== stationName) return;
    snapshot = weather;
    weatherCache.set(stationName, weather);
    loadedStationSummaries.add(stationName);
    renderStations();
    renderWeather(weather);
    if (animateContent) animateStateChange(weatherContent);
    if (weather.stale) {
      showDataUpdateFailure("上游更新未成功，已回退到缓存", animate);
      log(`${stationName} 更新失败，继续显示缓存`, "warning");
    } else {
      clearDataUpdateFailure(animate);
      log(`${stationName} 天气${refresh ? "已刷新" : "已载入"}`);
    }
  } catch (error) {
    if (requestToken !== weatherRequestToken || selectedStation !== stationName) return;
    const message = errorMessage(error);
    showDataUpdateFailure(message, animate);
    toast(`天气加载失败：${message}`, "error", animate);
    log(`刷新失败：${message}`, "error");
  } finally {
    if (requestToken === weatherRequestToken) {
      button.disabled = false;
      button.classList.remove("spinning");
      setWeatherLoading(false, animateContent);
      renderStations();
    }
  }
}

function setWeatherLoading(loading: boolean, showContentLoading = true): void {
  weatherLoading = loading;
  weatherContent.setAttribute("aria-busy", String(loading));
  weatherContent.dataset.loading = String(loading);
  weatherContent.dataset.loadingVisual = String(loading && showContentLoading);
}

const value = (input: unknown, suffix = "", digits = 0): string =>
  typeof input === "number" && Number.isFinite(input) ? `${input.toFixed(digits)}${suffix}` : "—";

const text = (input: string | null | undefined): string => input?.trim() || "—";

function clockTime(input: string | null | undefined): string {
  const match = input?.trim().match(/(\d{1,2}):(\d{2})(?::(\d{2}))?/);
  if (!match) return "—";
  const hour = match[1];
  const minute = match[2];
  const second = match[3] ?? "00";
  if (!hour || !minute) return "—";
  if (Number(hour) > 23 || Number(minute) > 59 || Number(second) > 59) return "—";
  return `${hour.padStart(2, "0")}:${minute}`;
}

function currentPressure(weather: WeatherSnapshot | null): number | null | undefined {
  const observed = weather?.real?.air_pressure;
  if (typeof observed === "number" && Number.isFinite(observed)) return observed;
  return weather?.passedchart?.find(
    ({ pressure }) => typeof pressure === "number" && Number.isFinite(pressure),
  )?.pressure;
}

function forecastWeatherDescription(day: ForecastDay | null | undefined): string | null {
  return usableWeatherDescription(day?.day_info, day?.night_info);
}

function currentWeatherDescription(weather: WeatherSnapshot | null | undefined): string | null {
  const today = weather?.predict?.days[0];
  return usableWeatherDescription(
    weather?.real?.info,
    today?.day_info,
    today?.night_info,
  );
}

function renderWeather(weather: WeatherSnapshot | null, placeholder = false): void {
  const current = weather?.real;
  const station = weather?.station;
  const firstDay = weather?.predict?.days[0];
  const currentDescription = currentWeatherDescription(weather);
  document.documentElement.dataset.weather = placeholder
    ? "unknown"
    : weatherAtmosphere(currentDescription);
  element("location").textContent = station?.name || selectedStation || "—";
  element("publish-time").textContent = placeholder
    ? "观测时间 —"
    : current?.publish_time
      ? `观测时间 ${current.publish_time}`
      : "等待天气数据";
  element("temperature").textContent = value(current?.temperature, "°", 1);
  element("weather-info").textContent = placeholder ? "—" : text(currentDescription);
  const weatherDescription = placeholder
    ? "天气信息加载中"
    : currentDescription || "暂无天气信息";
  const weatherIcon = element<HTMLImageElement>("weather-icon");
  weatherIcon.src = weatherAsset(currentDescription);
  element("weather-icon-tooltip").textContent = weatherDescription;
  element("weather-icon-wrap").setAttribute("aria-label", `当前天气：${weatherDescription}`);
  element("stale-badge").hidden = !weather?.stale;

  element("high-low").textContent = `最高 ${text(firstDay?.day_temperature)}° · 最低 ${text(firstDay?.night_temperature)}°`;
  renderComfort(placeholder ? "—" : current?.comfort_label, placeholder ? "—" : current?.comfort_index);

  renderCurrentMetrics(weather);
  renderAlert(weather, placeholder);
  renderForecast(weather, placeholder);
  renderAir(weather, placeholder);
  renderHistory(weather, placeholder);
  renderExtra(weather, placeholder);
}

function renderComfort(label: string | null | undefined, index: string | null | undefined): void {
  const comfort = element<HTMLDivElement>("comfort");
  const entries = [
    { icon: "comfort", label: "舒适度", value: label },
    { icon: "gauge", label: "指数", value: index },
  ] as const;
  comfort.replaceChildren();
  for (const entry of entries) {
    const metric = entry.value?.trim();
    if (!metric) continue;
    const item = document.createElement("span");
    item.className = "comfort-item";
    item.title = `${entry.label}：${metric}`;
    const icon = uiIcon(entry.icon, "comfort-icon");
    const name = document.createElement("span");
    name.textContent = entry.label;
    const value = document.createElement("strong");
    value.textContent = metric;
    item.append(icon, name, value);
    comfort.append(item);
  }
  comfort.hidden = comfort.childElementCount === 0;
}

function renderCurrentMetrics(weather: WeatherSnapshot | null): void {
  const current = weather?.real;
  const metrics: Array<[string, string]> = [
    ["体感", value(current?.feel_temperature, "°", 1)],
    ["湿度", value(current?.humidity, "%", 0)],
    ["降水", value(current?.rain, " mm", 1)],
    ["气压", value(currentPressure(weather), " hPa", 0)],
    ["风向风力", [current?.wind_direct, current?.wind_power].filter(Boolean).join(" ") || "—"],
    ["风速", value(current?.wind_speed, " m/s", 1)],
    ["日出", clockTime(current?.sunrise)],
    ["日落", clockTime(current?.sunset)],
  ];
  const target = element<HTMLDivElement>("current-metrics");
  target.replaceChildren();
  for (const [label, metric] of metrics) {
    const node = document.createElement("div");
    const small = document.createElement("small");
    small.textContent = label;
    const strong = document.createElement("strong");
    strong.textContent = metric;
    node.append(small, strong);
    target.append(node);
  }
}

function renderAlert(weather: WeatherSnapshot | null, placeholder = false): void {
  const alerts = weather?.real?.alerts ?? [];
  const card = element<HTMLElement>("alert-card");
  const list = element<HTMLDivElement>("alert-list");
  list.replaceChildren();
  if (placeholder || !alerts.length) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  alerts.forEach((alert) => {
    const item = document.createElement("article");
    item.className = "alert-item";
    const heading = document.createElement("div");
    heading.className = "alert-item-heading";
    const title = document.createElement("h3");
    title.textContent = [alert.alert, alert.signal_level].filter(Boolean).join(" · ") || "天气预警";
    const source = document.createElement("span");
    source.className = "alert-source";
    source.textContent = alert.inherited ? "父级站点" : "当前站点";
    heading.append(title, source);
    const location = [alert.province, alert.city]
      .filter((value, locationIndex, values) => value && values.indexOf(value) === locationIndex)
      .join(" · ");
    if (location) {
      const locationLine = document.createElement("small");
      locationLine.className = "alert-location";
      locationLine.textContent = location;
      heading.append(locationLine);
    }
    const content = document.createElement("p");
    content.textContent = text(alert.issue_content);
    item.append(heading, content);
    if (alert.prevention) {
      const prevention = document.createElement("p");
      prevention.className = "muted alert-prevention";
      prevention.textContent = `防御建议：${alert.prevention}`;
      item.append(prevention);
    }
    list.append(item);
  });
}

function renderForecast(weather: WeatherSnapshot | null, placeholder = false): void {
  const forecast = weather?.predict;
  element("forecast-time").textContent = forecast?.publish_time ? `发布 ${forecast.publish_time}` : "";
  const target = element<HTMLDivElement>("forecast-list");
  target.replaceChildren();
  const days = placeholder ? Array.from({ length: 6 }, () => null) : forecast?.days ?? [];
  if (!days.length) {
    target.append(emptyCopy("暂无预报数据"));
    return;
  }
  days.forEach((day) => {
    const card = document.createElement("article");
    card.className = "forecast-day";
    const date = document.createElement("time");
    date.textContent = placeholder ? "—" : multiDayDateLabel(day?.date);
    const dateTime = calendarDateIso(day?.date);
    if (dateTime) date.dateTime = dateTime;
    const description = forecastWeatherDescription(day);
    card.dataset.weather = weatherAtmosphere(description);
    const icon = document.createElement("img");
    icon.src = weatherAsset(description);
    icon.alt = placeholder ? "天气占位图标" : text(description);
    const info = document.createElement("strong");
    info.textContent = text(description);
    const temp = document.createElement("span");
    temp.textContent = `${text(day?.day_temperature)}° / ${text(day?.night_temperature)}°`;
    const wind = document.createElement("small");
    wind.textContent = [day?.day_wind_direct || day?.wind_direct, day?.day_wind_power || day?.wind_power].filter(Boolean).join(" ") || "—";
    card.append(date, icon, info, temp, wind);
    target.append(card);
  });
}

function renderAir(weather: WeatherSnapshot | null, placeholder = false): void {
  const air = weather?.air;
  const aqi = element("aqi");
  aqi.textContent = value(air?.aqi);
  aqi.dataset.level = aqiLevel(air?.aqi);
  const target = element<HTMLDivElement>("air-content");
  target.replaceChildren();
  if (placeholder) {
    const summary = document.createElement("p");
    summary.className = "air-summary";
    summary.textContent = "—";
    const grid = document.createElement("div");
    grid.className = "pollutant-grid";
    for (const label of ["PM2.5", "PM10", "NO₂", "SO₂", "CO", "O₃"]) {
      const item = document.createElement("span");
      item.textContent = `${label} —`;
      grid.append(item);
    }
    target.append(summary, grid);
    return;
  }
  if (!air) {
    target.append(emptyCopy("暂无空气质量数据"));
    return;
  }
  const summary = document.createElement("p");
  summary.className = "air-summary";
  summary.textContent = [air.category, air.level, air.primary_pollutant ? `首要污染物 ${air.primary_pollutant}` : ""].filter(Boolean).join(" · ") || "暂无评级";
  const grid = document.createElement("div");
  grid.className = "pollutant-grid";
  for (const [label, reading] of [["PM2.5", air.pm2_5], ["PM10", air.pm10], ["NO₂", air.no2], ["SO₂", air.so2], ["CO", air.co], ["O₃", air.o3]] as const) {
    const item = document.createElement("span");
    item.textContent = `${label} ${value(reading)}`;
    grid.append(item);
  }
  target.append(summary, grid);
}

function aqiLevel(aqi?: number | null): string {
  if (typeof aqi !== "number") return "none";
  if (aqi <= 50) return "good";
  if (aqi <= 100) return "moderate";
  return "poor";
}

function renderHistory(weather: WeatherSnapshot | null, placeholder = false): void {
  historyInteractionCleanup?.();
  historyInteractionCleanup = null;
  const rows = recentTemperatureHistoryRows(weather?.passedchart ?? []);
  const target = element<HTMLDivElement>("history-chart");
  target.replaceChildren();
  element("history-time").textContent = rows.length
    ? multiDayDateTimeLabel(rows.at(-1)?.time)
    : "";
  if (placeholder) {
    const chart = document.createElement("div");
    chart.className = "chart-placeholder";
    chart.textContent = "—";
    target.append(chart);
    return;
  }
  if (rows.length < 2) {
    target.append(emptyCopy("暂无连续观测数据"));
    return;
  }
  const temperatures = rows.map((row) => row.temperature as number);
  const min = Math.min(...temperatures);
  const max = Math.max(...temperatures);
  const span = Math.max(max - min, 1);
  const plotLeft = 28;
  const plotRight = 319;
  const plotTop = 12;
  const plotBottom = 82;
  const chartPoints = temperatures.map((temperature, index) => {
    const x = plotLeft + (index / (temperatures.length - 1)) * (plotRight - plotLeft);
    const y = max === min
      ? (plotTop + plotBottom) / 2
      : plotBottom - ((temperature - min) / span) * (plotBottom - plotTop);
    return { x, y };
  });
  const plotPoints = savitzkyGolayTemperaturePlotSamples(temperatures).map((sample) => {
    const x = plotLeft + (sample.position / (temperatures.length - 1)) * (plotRight - plotLeft);
    const y = max === min
      ? (plotTop + plotBottom) / 2
      : plotBottom - ((sample.temperature - min) / span) * (plotBottom - plotTop);
    return { x, y };
  });
  const points = plotPoints.map(({ x, y }) => `${x.toFixed(1)},${y.toFixed(1)}`);
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 320 112");
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", `过去温度趋势，最低 ${min.toFixed(1)}°，最高 ${max.toFixed(1)}°，悬停或长按查看各时段气温`);
  svg.setAttribute("tabindex", "0");
  const axes = document.createElementNS(svg.namespaceURI, "g");
  axes.setAttribute("class", "chart-axes");
  axes.setAttribute("aria-hidden", "true");
  const axisLabels = document.createElement("div");
  axisLabels.className = "chart-axis-labels";
  axisLabels.setAttribute("aria-hidden", "true");
  for (const [temperature, y] of [[max, plotTop], [min, plotBottom]] as const) {
    const gridLine = document.createElementNS(svg.namespaceURI, "line");
    gridLine.setAttribute("class", "chart-range-line");
    gridLine.setAttribute("x1", String(plotLeft));
    gridLine.setAttribute("x2", String(plotRight));
    gridLine.setAttribute("y1", String(y));
    gridLine.setAttribute("y2", String(y));
    const label = document.createElement("span");
    label.className = "chart-range-label";
    label.style.left = `${((plotLeft - 4) / 320) * 100}%`;
    label.style.top = `${(y / 112) * 100}%`;
    label.textContent = `${temperature.toFixed(1)}°`;
    axes.append(gridLine);
    axisLabels.append(label);
  }
  for (const index of temperatureHistoryTickIndices(rows.length)) {
    const chartPoint = chartPoints[index];
    const row = rows[index];
    if (!chartPoint || !row) continue;
    const tick = document.createElementNS(svg.namespaceURI, "line");
    tick.setAttribute("class", "chart-time-tick");
    tick.setAttribute("x1", String(chartPoint.x));
    tick.setAttribute("x2", String(chartPoint.x));
    tick.setAttribute("y1", String(plotBottom));
    tick.setAttribute("y2", String(plotBottom + 5));
    const label = document.createElement("span");
    label.className = "chart-time-label";
    label.style.left = `${(chartPoint.x / 320) * 100}%`;
    label.style.top = `${(102 / 112) * 100}%`;
    label.textContent = clockTime(row.time);
    axes.append(tick);
    axisLabels.append(label);
  }
  const area = document.createElementNS(svg.namespaceURI, "polygon");
  area.setAttribute("points", `${plotLeft},${plotBottom} ${points.join(" ")} ${plotRight},${plotBottom}`);
  area.setAttribute("class", "chart-area");
  const line = document.createElementNS(svg.namespaceURI, "polyline");
  line.setAttribute("points", points.join(" "));
  line.setAttribute("class", "chart-line");
  const inspector = document.createElementNS("http://www.w3.org/2000/svg", "g");
  inspector.setAttribute("class", "chart-inspector");
  inspector.setAttribute("aria-hidden", "true");
  const guide = document.createElementNS("http://www.w3.org/2000/svg", "line");
  guide.setAttribute("class", "chart-inspector-guide");
  guide.setAttribute("y1", String(plotTop));
  guide.setAttribute("y2", String(plotBottom));
  const halo = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  halo.setAttribute("class", "chart-inspector-halo");
  halo.setAttribute("r", "7");
  const point = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  point.setAttribute("class", "chart-inspector-point");
  point.setAttribute("r", "3.5");
  inspector.append(guide, halo, point);
  svg.append(axes, area, line, inspector);
  const tooltip = document.createElement("div");
  tooltip.id = "history-chart-tooltip";
  tooltip.className = "chart-tooltip";
  tooltip.setAttribute("role", "tooltip");
  tooltip.hidden = true;
  const tooltipTime = document.createElement("time");
  const tooltipTemperature = document.createElement("strong");
  tooltip.append(tooltipTime, tooltipTemperature);
  const legend = document.createElement("div");
  legend.className = "chart-legend";
  const interactionHint = document.createElement("span");
  interactionHint.className = "chart-interaction-hint";
  interactionHint.textContent = "悬停 / 长按查看";
  legend.append(interactionHint);
  target.append(svg, axisLabels, tooltip, legend);
  animateChartPaths([line as SVGGeometryElement], [area]);
  historyInteractionCleanup = bindHistoryChartInteractions({
    target,
    svg,
    rows,
    chartPoints,
    inspector,
    guide,
    halo,
    point,
    tooltip,
    tooltipTime,
    tooltipTemperature,
  });
}

type HistoryChartInteraction = {
  target: HTMLDivElement;
  svg: SVGSVGElement;
  rows: WeatherSnapshot["passedchart"];
  chartPoints: Array<{ x: number; y: number }>;
  inspector: SVGGElement;
  guide: SVGLineElement;
  halo: SVGCircleElement;
  point: SVGCircleElement;
  tooltip: HTMLDivElement;
  tooltipTime: HTMLTimeElement;
  tooltipTemperature: HTMLElement;
};

function bindHistoryChartInteractions(chart: HistoryChartInteraction): () => void {
  const {
    target,
    svg,
    rows,
    chartPoints,
    inspector,
    guide,
    halo,
    point,
    tooltip,
    tooltipTime,
    tooltipTemperature,
  } = chart;
  const baseLabel = svg.getAttribute("aria-label") ?? "过去温度趋势，悬停或长按查看各时段气温";
  const LONG_PRESS_MS = 420;
  const LONG_PRESS_MOVE_TOLERANCE = 10;
  let activeIndex = rows.length - 1;
  let longPressTimer: number | undefined;
  let touchPointerId: number | null = null;
  let touchStartX = 0;
  let touchStartY = 0;
  let touchClientX = 0;
  let longPressActive = false;

  const clearLongPressTimer = (): void => {
    if (longPressTimer !== undefined) window.clearTimeout(longPressTimer);
    longPressTimer = undefined;
  };
  const hideInspector = (): void => {
    inspector.classList.remove("visible");
    tooltip.hidden = true;
    delete target.dataset.inspecting;
    svg.setAttribute("aria-label", baseLabel);
  };
  const showIndex = (index: number): void => {
    const row = rows[index];
    const chartPoint = chartPoints[index];
    if (!row || !chartPoint || typeof row.temperature !== "number") return;
    const clientPoint = svgUserPointToClient(svg, chartPoint.x, chartPoint.y);
    if (!clientPoint) return;
    activeIndex = index;
    const time = row.time?.trim()
      ? multiDayDateTimeLabel(row.time)
      : `第 ${index + 1} 个时段`;
    const temperature = `${row.temperature.toFixed(1)}°`;
    const x = chartPoint.x.toFixed(1);
    const y = chartPoint.y.toFixed(1);
    guide.setAttribute("x1", x);
    guide.setAttribute("x2", x);
    halo.setAttribute("cx", x);
    halo.setAttribute("cy", y);
    point.setAttribute("cx", x);
    point.setAttribute("cy", y);
    tooltipTime.textContent = time;
    tooltipTemperature.textContent = temperature;
    tooltip.hidden = false;
    inspector.classList.add("visible");
    target.dataset.inspecting = "true";
    svg.setAttribute("aria-label", `过去温度趋势，${time} 气温 ${temperature}`);

    const targetBounds = target.getBoundingClientRect();
    const rawLeft = clientPoint.x - targetBounds.left;
    const tooltipHalfWidth = Math.min(tooltip.offsetWidth / 2, Math.max(0, target.clientWidth / 2 - 4));
    tooltip.style.left = `${clamp(rawLeft, tooltipHalfWidth + 4, target.clientWidth - tooltipHalfWidth - 4)}px`;
    const pointTop = clientPoint.y - targetBounds.top;
    const above = pointTop - tooltip.offsetHeight - 10;
    const placeBelow = above < 2;
    tooltip.dataset.position = placeBelow ? "below" : "above";
    tooltip.style.top = `${placeBelow ? pointTop + 10 : above}px`;
  };
  const showClientX = (clientX: number): void => {
    const bounds = svg.getBoundingClientRect();
    if (bounds.width <= 0) return;
    const userPoint = svgClientPointToUser(svg, clientX, bounds.top + bounds.height / 2);
    if (!userPoint) return;
    showIndex(nearestPlotPointIndex(chartPoints, userPoint.x));
  };
  const finishTouch = (event: PointerEvent): void => {
    if (event.pointerId !== touchPointerId) return;
    clearLongPressTimer();
    const wasActive = longPressActive;
    longPressActive = false;
    touchPointerId = null;
    if (svg.hasPointerCapture(event.pointerId)) svg.releasePointerCapture(event.pointerId);
    if (wasActive) hideInspector();
  };

  svg.addEventListener("pointerdown", (event) => {
    if (event.pointerType === "mouse" || event.button !== 0) return;
    clearLongPressTimer();
    touchPointerId = event.pointerId;
    touchStartX = event.clientX;
    touchStartY = event.clientY;
    touchClientX = event.clientX;
    longPressActive = false;
    longPressTimer = window.setTimeout(() => {
      longPressTimer = undefined;
      if (touchPointerId !== event.pointerId || !svg.isConnected) return;
      longPressActive = true;
      svg.setPointerCapture(event.pointerId);
      showClientX(touchClientX);
    }, LONG_PRESS_MS);
  });
  svg.addEventListener("pointermove", (event) => {
    if (event.pointerType === "mouse") {
      if (event.buttons === 0) showClientX(event.clientX);
      return;
    }
    if (event.pointerId !== touchPointerId) return;
    touchClientX = event.clientX;
    if (longPressActive) {
      event.preventDefault();
      showClientX(event.clientX);
      return;
    }
    if (Math.hypot(event.clientX - touchStartX, event.clientY - touchStartY) > LONG_PRESS_MOVE_TOLERANCE) {
      clearLongPressTimer();
      touchPointerId = null;
    }
  });
  svg.addEventListener("pointerleave", (event) => {
    if (event.pointerType === "mouse") hideInspector();
  });
  svg.addEventListener("pointerup", finishTouch);
  svg.addEventListener("pointercancel", finishTouch);
  svg.addEventListener("lostpointercapture", finishTouch);
  svg.addEventListener("focus", () => {
    if (svg.matches(":focus-visible")) showIndex(activeIndex);
  });
  svg.addEventListener("blur", () => {
    if (!longPressActive) hideInspector();
  });
  svg.addEventListener("keydown", (event) => {
    let nextIndex = activeIndex;
    if (event.key === "ArrowLeft") nextIndex -= 1;
    else if (event.key === "ArrowRight") nextIndex += 1;
    else if (event.key === "Home") nextIndex = 0;
    else if (event.key === "End") nextIndex = rows.length - 1;
    else if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    showIndex(clamp(nextIndex, 0, rows.length - 1));
  });
  const handleResize = (): void => hideInspector();
  window.addEventListener("resize", handleResize);

  return () => {
    clearLongPressTimer();
    window.removeEventListener("resize", handleResize);
  };
}

type DailyTemperaturePlotPoint = {
  data: DailyTemperaturePoint;
  x: number;
  maxY: number | null;
  minY: number | null;
};

const DAILY_TEMPERATURE_HISTORY_DAYS = 2;
const DAILY_TEMPERATURE_FORECAST_DAYS = 7;
const DAILY_TEMPERATURE_HORIZONTAL_PADDING = 30;
const DAILY_TEMPERATURE_MIN_VIEWPORT_WIDTH = 320;
const DAILY_TEMPERATURE_INITIAL_PAGE_SIZE = 7;
const DAILY_TEMPERATURE_NEXT_PAGE_SIZE = 31;

type DailyTemperatureRenderOptions = {
  initialInterval?: number;
  onReachPast?: () => void;
};

type DailyTemperatureChartInteraction = {
  target: HTMLDivElement;
  scroll: HTMLDivElement;
  svg: SVGSVGElement;
  chartWidth: number;
  points: DailyTemperaturePlotPoint[];
  inspector: SVGGElement;
  guide: SVGLineElement;
  maxHalo: SVGCircleElement;
  maxPoint: SVGCircleElement;
  minHalo: SVGCircleElement;
  minPoint: SVGCircleElement;
  tooltip: HTMLDivElement;
  tooltipDate: HTMLTimeElement;
  tooltipSource: HTMLElement;
  tooltipMax: HTMLElement;
  tooltipMin: HTMLElement;
};

function renderDailyTemperatureChart(
  target: HTMLDivElement,
  input: DailyTemperaturePoint[],
  options: DailyTemperatureRenderOptions = {},
): void {
  dailyTemperatureInteractionCleanup?.();
  dailyTemperatureInteractionCleanup = null;
  target.replaceChildren();
  target.setAttribute("aria-busy", "false");
  const points = input
    .filter((point) => point.date.trim()
      && (isFiniteTemperature(point.max_temperature) || isFiniteTemperature(point.min_temperature)))
    .slice()
    .sort((left, right) => left.date.localeCompare(right.date));
  if (!points.length) {
    target.append(emptyCopy("暂无历史/预报气温数据"));
    return;
  }

  const values = points.flatMap((point) => [point.max_temperature, point.min_temperature])
    .filter(isFiniteTemperature);
  const minimum = Math.min(...values);
  const maximum = Math.max(...values);
  const temperatureSpan = Math.max(maximum - minimum, 1);
  const firstForecastIndex = points.findIndex((point) => point.forecast);
  const anchorIndex = firstForecastIndex >= 0 ? firstForecastIndex : points.length - 1;
  const defaultStartIndex = Math.max(0, anchorIndex - DAILY_TEMPERATURE_HISTORY_DAYS);
  const defaultEndIndex = Math.min(points.length - 1, anchorIndex + DAILY_TEMPERATURE_FORECAST_DAYS);
  const defaultIntervals = Math.max(1, defaultEndIndex - defaultStartIndex);
  const viewportWidth = Math.max(target.clientWidth, DAILY_TEMPERATURE_MIN_VIEWPORT_WIDTH);
  const viewportPlotWidth = Math.max(
    1,
    viewportWidth - DAILY_TEMPERATURE_HORIZONTAL_PADDING * 2,
  );
  const pointSpacing = viewportPlotWidth / defaultIntervals;
  const chartWidth = Math.max(
    viewportWidth,
    DAILY_TEMPERATURE_HORIZONTAL_PADDING * 2
      + Math.max(0, points.length - 1) * pointSpacing,
  );
  const chartHeight = 148;
  const chartTop = 16;
  const chartBottom = 110;
  const scaleY = (temperature: number): number =>
    chartBottom - ((temperature - minimum) / temperatureSpan) * (chartBottom - chartTop);
  const plotPoints: DailyTemperaturePlotPoint[] = points.map((data, index) => ({
    data,
    x: points.length === 1
      ? chartWidth / 2
      : DAILY_TEMPERATURE_HORIZONTAL_PADDING + index * pointSpacing,
    maxY: isFiniteTemperature(data.max_temperature) ? scaleY(data.max_temperature) : null,
    minY: isFiniteTemperature(data.min_temperature) ? scaleY(data.min_temperature) : null,
  }));

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${chartWidth} ${chartHeight}`);
  svg.setAttribute("width", String(chartWidth));
  svg.setAttribute("height", String(chartHeight));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "每日最高和最低气温变化，悬停或长按查看各日期气温");
  svg.setAttribute("tabindex", "0");

  if (firstForecastIndex >= 0) {
    const boundaryX = firstForecastIndex === 0
      ? 0
      : (plotPoints[firstForecastIndex - 1]!.x + plotPoints[firstForecastIndex]!.x) / 2;
    const forecastRegion = document.createElementNS(svg.namespaceURI, "rect");
    forecastRegion.setAttribute("class", "daily-forecast-region");
    forecastRegion.setAttribute("x", boundaryX.toFixed(1));
    forecastRegion.setAttribute("y", "8");
    forecastRegion.setAttribute("width", (chartWidth - boundaryX).toFixed(1));
    forecastRegion.setAttribute("height", "108");
    const boundary = document.createElementNS(svg.namespaceURI, "line");
    boundary.setAttribute("class", "daily-forecast-boundary");
    boundary.setAttribute("x1", boundaryX.toFixed(1));
    boundary.setAttribute("x2", boundaryX.toFixed(1));
    boundary.setAttribute("y1", "8");
    boundary.setAttribute("y2", "116");
    svg.append(forecastRegion, boundary);
  }

  for (const ratio of [0, 0.5, 1]) {
    const y = chartBottom - ratio * (chartBottom - chartTop);
    const grid = document.createElementNS(svg.namespaceURI, "line");
    grid.setAttribute("class", "daily-temperature-grid");
    grid.setAttribute("x1", "25");
    grid.setAttribute("x2", String(chartWidth - 12));
    grid.setAttribute("y1", y.toFixed(1));
    grid.setAttribute("y2", y.toFixed(1));
    const label = document.createElementNS(svg.namespaceURI, "text");
    label.setAttribute("class", "daily-temperature-y-label");
    label.setAttribute("x", "2");
    label.setAttribute("y", (y + 3).toFixed(1));
    label.textContent = `${(minimum + ratio * temperatureSpan).toFixed(0)}°`;
    svg.append(grid, label);
  }

  const maxLine = document.createElementNS(svg.namespaceURI, "path");
  maxLine.setAttribute("class", "daily-temperature-line daily-temperature-max-line");
  maxLine.setAttribute("d", dailyTemperaturePath(plotPoints, "maxY"));
  const minLine = document.createElementNS(svg.namespaceURI, "path");
  minLine.setAttribute("class", "daily-temperature-line daily-temperature-min-line");
  minLine.setAttribute("d", dailyTemperaturePath(plotPoints, "minY"));
  svg.append(maxLine, minLine);

  for (const plotPoint of plotPoints) {
    const dateLabel = document.createElementNS(svg.namespaceURI, "text");
    dateLabel.setAttribute("class", "daily-temperature-date-label");
    dateLabel.setAttribute("x", plotPoint.x.toFixed(1));
    dateLabel.setAttribute("y", "137");
    dateLabel.textContent = multiDayDateLabel(plotPoint.data.date);
    svg.append(dateLabel);
    for (const [y, className] of [
      [plotPoint.maxY, "daily-temperature-dot daily-temperature-max-dot"],
      [plotPoint.minY, "daily-temperature-dot daily-temperature-min-dot"],
    ] as const) {
      if (y === null) continue;
      const dot = document.createElementNS(svg.namespaceURI, "circle");
      dot.setAttribute("class", className);
      dot.setAttribute("cx", plotPoint.x.toFixed(1));
      dot.setAttribute("cy", y.toFixed(1));
      dot.setAttribute("r", "2.5");
      svg.append(dot);
    }
  }

  const inspector = document.createElementNS("http://www.w3.org/2000/svg", "g");
  inspector.setAttribute("class", "chart-inspector");
  inspector.setAttribute("aria-hidden", "true");
  const guide = document.createElementNS("http://www.w3.org/2000/svg", "line");
  guide.setAttribute("class", "chart-inspector-guide");
  guide.setAttribute("y1", "8");
  guide.setAttribute("y2", "116");
  const maxHalo = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  maxHalo.setAttribute("class", "chart-inspector-halo daily-temperature-max-halo");
  maxHalo.setAttribute("r", "7");
  const maxPoint = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  maxPoint.setAttribute("class", "chart-inspector-point daily-temperature-max-point");
  maxPoint.setAttribute("r", "3.5");
  const minHalo = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  minHalo.setAttribute("class", "chart-inspector-halo daily-temperature-min-halo");
  minHalo.setAttribute("r", "7");
  const minPoint = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  minPoint.setAttribute("class", "chart-inspector-point daily-temperature-min-point");
  minPoint.setAttribute("r", "3.5");
  inspector.append(guide, maxHalo, maxPoint, minHalo, minPoint);
  svg.append(inspector);

  const scroll = document.createElement("div");
  scroll.className = "daily-temperature-scroll";
  scroll.dataset.pointSpacing = String(pointSpacing);
  scroll.append(svg);
  const tooltip = document.createElement("div");
  tooltip.className = "chart-tooltip daily-temperature-tooltip";
  tooltip.setAttribute("role", "tooltip");
  tooltip.hidden = true;
  const tooltipHeading = document.createElement("span");
  tooltipHeading.className = "daily-temperature-tooltip-heading";
  const tooltipDate = document.createElement("time");
  const tooltipSource = document.createElement("small");
  tooltipHeading.append(tooltipDate, tooltipSource);
  const tooltipMaxRow = document.createElement("span");
  tooltipMaxRow.textContent = "最高 ";
  const tooltipMax = document.createElement("strong");
  tooltipMaxRow.append(tooltipMax);
  const tooltipMinRow = document.createElement("span");
  tooltipMinRow.textContent = "最低 ";
  const tooltipMin = document.createElement("strong");
  tooltipMinRow.append(tooltipMin);
  tooltip.append(tooltipHeading, tooltipMaxRow, tooltipMinRow);

  const legend = document.createElement("div");
  legend.className = "chart-legend daily-temperature-legend";
  const seriesLegend = document.createElement("span");
  seriesLegend.className = "daily-temperature-series-legend";
  seriesLegend.innerHTML = "<i class=\"max\"></i>最高温 <i class=\"min\"></i>最低温";
  const interactionHint = document.createElement("span");
  interactionHint.className = "chart-interaction-hint";
  interactionHint.textContent = "左右滑动 · 悬停 / 长按查看";
  legend.append(seriesLegend, interactionHint);
  target.append(scroll, tooltip, legend);
  if (options.initialInterval === undefined) {
    animateChartPaths([maxLine as SVGGeometryElement, minLine as SVGGeometryElement]);
  }

  const cleanupInteractions = bindDailyTemperatureChartInteractions({
    target,
    scroll,
    svg,
    chartWidth,
    points: plotPoints,
    inspector,
    guide,
    maxHalo,
    maxPoint,
    minHalo,
    minPoint,
    tooltip,
    tooltipDate,
    tooltipSource,
    tooltipMax,
    tooltipMin,
  });

  const handleScroll = (): void => {
    if (scroll.scrollLeft <= pointSpacing * 0.75) options.onReachPast?.();
  };
  scroll.addEventListener("scroll", handleScroll, { passive: true });

  let resizeFrame: number | undefined;
  const renderedWidth = target.clientWidth;
  const resizeObserver = new ResizeObserver((entries) => {
    const nextWidth = entries[0]?.contentRect.width ?? target.clientWidth;
    if (Math.abs(nextWidth - renderedWidth) < 1 || resizeFrame !== undefined) return;
    resizeFrame = window.requestAnimationFrame(() => {
      resizeFrame = undefined;
      if (target.isConnected) {
        renderDailyTemperatureChart(target, input, {
          ...options,
          initialInterval: scroll.scrollLeft / pointSpacing,
        });
      }
    });
  });
  resizeObserver.observe(target);
  dailyTemperatureInteractionCleanup = () => {
    cleanupInteractions();
    scroll.removeEventListener("scroll", handleScroll);
    resizeObserver.disconnect();
    if (resizeFrame !== undefined) window.cancelAnimationFrame(resizeFrame);
  };

  window.requestAnimationFrame(() => {
    if (!scroll.isConnected) return;
    scroll.scrollLeft = clamp(
      (options.initialInterval ?? defaultStartIndex) * pointSpacing,
      0,
      Math.max(0, scroll.scrollWidth - scroll.clientWidth),
    );
    if (scroll.scrollLeft <= pointSpacing * 0.75) options.onReachPast?.();
  });
}

function mergeDailyTemperaturePoints(
  current: DailyTemperaturePoint[],
  incoming: DailyTemperaturePoint[],
): DailyTemperaturePoint[] {
  const merged = new Map<string, DailyTemperaturePoint>();
  for (const point of [...current, ...incoming]) {
    if (!point.date.trim()) continue;
    const existing = merged.get(point.date);
    merged.set(point.date, {
      date: point.date,
      max_temperature: point.max_temperature ?? existing?.max_temperature ?? null,
      min_temperature: point.min_temperature ?? existing?.min_temperature ?? null,
      forecast: point.forecast || existing?.forecast === true,
    });
  }
  return [...merged.values()].sort((left, right) => left.date.localeCompare(right.date));
}

function dailyTemperatureScrollInterval(target: HTMLDivElement): number {
  const scroll = target.querySelector<HTMLDivElement>(".daily-temperature-scroll");
  if (!scroll) return 0;
  const spacing = Number(scroll.dataset.pointSpacing);
  return Number.isFinite(spacing) && spacing > 0 ? scroll.scrollLeft / spacing : 0;
}

function isFiniteTemperature(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function dailyTemperaturePath(
  points: DailyTemperaturePlotPoint[],
  key: "maxY" | "minY",
): string {
  let drawing = false;
  return points.map((point) => {
    const y = point[key];
    if (y === null) {
      drawing = false;
      return "";
    }
    const command = drawing ? "L" : "M";
    drawing = true;
    return `${command}${point.x.toFixed(1)},${y.toFixed(1)}`;
  }).filter(Boolean).join(" ");
}

function bindDailyTemperatureChartInteractions(
  chart: DailyTemperatureChartInteraction,
): () => void {
  const {
    target,
    scroll,
    svg,
    chartWidth,
    points,
    inspector,
    guide,
    maxHalo,
    maxPoint,
    minHalo,
    minPoint,
    tooltip,
    tooltipDate,
    tooltipSource,
    tooltipMax,
    tooltipMin,
  } = chart;
  const baseLabel = "每日最高和最低气温变化，悬停或长按查看各日期气温";
  const LONG_PRESS_MS = 420;
  const LONG_PRESS_MOVE_TOLERANCE = 10;
  const forecastIndex = points.findIndex(({ data }) => data.forecast);
  let activeIndex = forecastIndex >= 0 ? forecastIndex : points.length - 1;
  let longPressTimer: number | undefined;
  let touchPointerId: number | null = null;
  let touchStartX = 0;
  let touchStartY = 0;
  let touchClientX = 0;
  let longPressActive = false;

  const clearLongPressTimer = (): void => {
    if (longPressTimer !== undefined) window.clearTimeout(longPressTimer);
    longPressTimer = undefined;
  };
  const hideInspector = (): void => {
    inspector.classList.remove("visible");
    tooltip.hidden = true;
    delete target.dataset.inspecting;
    svg.setAttribute("aria-label", baseLabel);
  };
  const positionMarker = (
    halo: SVGCircleElement,
    point: SVGCircleElement,
    x: number,
    y: number | null,
  ): void => {
    halo.toggleAttribute("hidden", y === null);
    point.toggleAttribute("hidden", y === null);
    if (y === null) return;
    halo.setAttribute("cx", x.toFixed(1));
    halo.setAttribute("cy", y.toFixed(1));
    point.setAttribute("cx", x.toFixed(1));
    point.setAttribute("cy", y.toFixed(1));
  };
  const showIndex = (index: number): void => {
    const plotPoint = points[index];
    if (!plotPoint) return;
    activeIndex = index;
    const { data, x, maxY, minY } = plotPoint;
    guide.setAttribute("x1", x.toFixed(1));
    guide.setAttribute("x2", x.toFixed(1));
    positionMarker(maxHalo, maxPoint, x, maxY);
    positionMarker(minHalo, minPoint, x, minY);
    const maxTemperature = isFiniteTemperature(data.max_temperature)
      ? `${data.max_temperature.toFixed(1)}°`
      : "—";
    const minTemperature = isFiniteTemperature(data.min_temperature)
      ? `${data.min_temperature.toFixed(1)}°`
      : "—";
    const dateLabel = multiDayDateLabel(data.date);
    tooltipDate.textContent = dateLabel;
    tooltipDate.dateTime = calendarDateIso(data.date) ?? "";
    tooltipSource.textContent = data.forecast ? "预测" : "历史";
    tooltipMax.textContent = maxTemperature;
    tooltipMin.textContent = minTemperature;
    tooltip.hidden = false;
    inspector.classList.add("visible");
    target.dataset.inspecting = "true";
    svg.setAttribute(
      "aria-label",
      `${dateLabel} ${data.forecast ? "预测" : "历史"}，最高温 ${maxTemperature}，最低温 ${minTemperature}`,
    );

    const svgBounds = svg.getBoundingClientRect();
    const targetBounds = target.getBoundingClientRect();
    const rawLeft = svgBounds.left - targetBounds.left + (x / chartWidth) * svgBounds.width;
    const tooltipHalfWidth = Math.min(tooltip.offsetWidth / 2, Math.max(0, target.clientWidth / 2 - 4));
    tooltip.style.left = `${clamp(rawLeft, tooltipHalfWidth + 4, target.clientWidth - tooltipHalfWidth - 4)}px`;
    const visibleY = [maxY, minY].filter((value): value is number => value !== null);
    const pointY = visibleY.length ? Math.min(...visibleY) : 110;
    const pointTop = svgBounds.top - targetBounds.top + (pointY / 148) * svgBounds.height;
    const above = pointTop - tooltip.offsetHeight - 10;
    const placeBelow = above < 2;
    tooltip.dataset.position = placeBelow ? "below" : "above";
    tooltip.style.top = `${placeBelow ? pointTop + 10 : above}px`;
  };
  const showClientX = (clientX: number): void => {
    const bounds = svg.getBoundingClientRect();
    if (bounds.width <= 0) return;
    const ratio = clamp((clientX - bounds.left) / bounds.width, 0, 1);
    showIndex(Math.round(ratio * (points.length - 1)));
  };
  const ensureIndexVisible = (index: number): void => {
    const point = points[index];
    if (!point) return;
    const scale = svg.getBoundingClientRect().width / chartWidth;
    const contentX = point.x * scale;
    const left = scroll.scrollLeft + DAILY_TEMPERATURE_HORIZONTAL_PADDING;
    const right = scroll.scrollLeft + scroll.clientWidth - DAILY_TEMPERATURE_HORIZONTAL_PADDING;
    if (contentX < left) {
      scroll.scrollLeft = Math.max(0, contentX - DAILY_TEMPERATURE_HORIZONTAL_PADDING);
    } else if (contentX > right) {
      scroll.scrollLeft = Math.min(
        scroll.scrollWidth - scroll.clientWidth,
        contentX - scroll.clientWidth + DAILY_TEMPERATURE_HORIZONTAL_PADDING,
      );
    }
  };
  const finishTouch = (event: PointerEvent): void => {
    if (event.pointerId !== touchPointerId) return;
    clearLongPressTimer();
    const wasActive = longPressActive;
    longPressActive = false;
    touchPointerId = null;
    if (svg.hasPointerCapture(event.pointerId)) svg.releasePointerCapture(event.pointerId);
    if (wasActive) hideInspector();
  };

  svg.addEventListener("pointerdown", (event) => {
    if (event.pointerType === "mouse" || event.button !== 0) return;
    clearLongPressTimer();
    touchPointerId = event.pointerId;
    touchStartX = event.clientX;
    touchStartY = event.clientY;
    touchClientX = event.clientX;
    longPressActive = false;
    longPressTimer = window.setTimeout(() => {
      longPressTimer = undefined;
      if (touchPointerId !== event.pointerId || !svg.isConnected) return;
      longPressActive = true;
      svg.setPointerCapture(event.pointerId);
      showClientX(touchClientX);
    }, LONG_PRESS_MS);
  });
  svg.addEventListener("pointermove", (event) => {
    if (event.pointerType === "mouse") {
      if (event.buttons === 0) showClientX(event.clientX);
      return;
    }
    if (event.pointerId !== touchPointerId) return;
    touchClientX = event.clientX;
    if (longPressActive) {
      event.preventDefault();
      showClientX(event.clientX);
      return;
    }
    if (Math.hypot(event.clientX - touchStartX, event.clientY - touchStartY) > LONG_PRESS_MOVE_TOLERANCE) {
      clearLongPressTimer();
      touchPointerId = null;
    }
  });
  svg.addEventListener("pointerleave", (event) => {
    if (event.pointerType === "mouse") hideInspector();
  });
  svg.addEventListener("pointerup", finishTouch);
  svg.addEventListener("pointercancel", finishTouch);
  svg.addEventListener("lostpointercapture", finishTouch);
  svg.addEventListener("focus", () => {
    if (svg.matches(":focus-visible")) showIndex(activeIndex);
  });
  svg.addEventListener("blur", () => {
    if (!longPressActive) hideInspector();
  });
  svg.addEventListener("keydown", (event) => {
    let nextIndex = activeIndex;
    if (event.key === "ArrowLeft") nextIndex -= 1;
    else if (event.key === "ArrowRight") nextIndex += 1;
    else if (event.key === "Home") nextIndex = 0;
    else if (event.key === "End") nextIndex = points.length - 1;
    else if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    const boundedIndex = clamp(nextIndex, 0, points.length - 1);
    showIndex(boundedIndex);
    ensureIndexVisible(boundedIndex);
  });
  const handleResize = (): void => hideInspector();
  window.addEventListener("resize", handleResize);

  return () => {
    clearLongPressTimer();
    window.removeEventListener("resize", handleResize);
  };
}

function climatePeriodLabel(input: string | null | undefined): string {
  const period = input?.trim().replace(/\s*(?:气候)?常年值\s*$/u, "").trim();
  if (!period) return "—";
  const yearRange = period.match(/^(\d{4})\s*(?:-|–|—|~|至)\s*(\d{4})(?:年)?$/u);
  return yearRange ? `${yearRange[1]}—${yearRange[2]}` : period;
}

function climateMonthLabel(input: number | null | undefined): string {
  return typeof input === "number" && Number.isInteger(input) && input >= 1 && input <= 12
    ? `${input}月`
    : "—";
}

function climateValue(input: number | null | undefined, suffix: string): string {
  return typeof input === "number" && Number.isFinite(input)
    ? `${climateNumberFormat.format(input)}${suffix}`
    : "—";
}

function renderExtra(weather: WeatherSnapshot | null, placeholder = false): void {
  void closeImageViewer(false, false);
  dailyTemperatureInteractionCleanup?.();
  dailyTemperatureInteractionCleanup = null;
  const temperatureRequestToken = ++dailyTemperatureRequestToken;
  const renderToken = ++radarRenderToken;
  const target = element<HTMLDivElement>("extra-content");
  target.replaceChildren();
  const climate = document.createElement("section");
  climate.className = "climate-block";
  const climateHeader = document.createElement("header");
  climateHeader.className = "climate-header";
  const climateHeading = document.createElement("div");
  const climateKicker = document.createElement("span");
  climateKicker.className = "section-kicker";
  climateKicker.textContent = "气候资料";
  const climateTitle = document.createElement("h3");
  climateTitle.textContent = "气候常年值";
  climateHeading.append(climateKicker, climateTitle);
  const climatePeriod = document.createElement("small");
  climatePeriod.className = "climate-period";
  climatePeriod.textContent = placeholder ? "—" : climatePeriodLabel(weather?.climate?.period);
  climateHeader.append(climateHeading, climatePeriod);
  climate.append(climateHeader);
  const months = placeholder ? Array.from({ length: 12 }, () => null) : weather?.climate?.month ?? [];
  if (months.length) {
    const list = document.createElement("ol");
    list.className = "climate-months";
    for (const month of months.slice(0, 12)) {
      const item = document.createElement("li");
      item.className = "climate-month";
      const monthLabel = document.createElement("strong");
      monthLabel.className = "climate-month-label";
      monthLabel.textContent = climateMonthLabel(month?.month);
      const values = document.createElement("dl");
      values.className = "climate-values";
      const metrics = [
        ["平均高温", climateValue(month?.average_max_temperature, "°"), "high"],
        ["平均低温", climateValue(month?.average_min_temperature, "°"), "low"],
        ["降水", climateValue(month?.precipitation, " mm"), "rain"],
      ] as const;
      for (const [label, metric, kind] of metrics) {
        const row = document.createElement("div");
        row.dataset.metric = kind;
        const name = document.createElement("dt");
        name.textContent = label;
        const reading = document.createElement("dd");
        reading.textContent = metric;
        row.append(name, reading);
        values.append(row);
      }
      item.append(monthLabel, values);
      list.append(item);
    }
    climate.append(list);
  } else {
    climate.append(emptyCopy("暂无气候数据"));
  }

  const radar = document.createElement("div");
  radar.className = "radar-block";
  const image = document.createElement("img");
  const title = document.createElement("span");
  title.className = "radar-title";
  const zoomHint = document.createElement("span");
  zoomHint.className = "radar-zoom-hint";
  zoomHint.append(uiIcon("magnify"), document.createTextNode("放大"));
  image.src = assets.radarPlaceholder;
  image.alt = placeholder ? "雷达图占位图" : weather?.radar?.title || "雷达图占位图";
  image.addEventListener("error", () => {
    delete radar.dataset.zoomable;
    delete image.dataset.resourceId;
    radar.removeAttribute("role");
    radar.removeAttribute("tabindex");
    title.textContent = "雷达图加载失败";
    if (image.src !== assets.radarPlaceholder) image.src = assets.radarPlaceholder;
  }, { once: true });
  title.textContent = placeholder ? "—" : weather?.radar?.title || "暂无雷达图像";
  radar.append(image, title, zoomHint);
  const dailyTemperature = document.createElement("section");
  dailyTemperature.className = "daily-temperature-block";
  const dailyHeader = document.createElement("header");
  dailyHeader.className = "daily-temperature-header";
  const dailyHeading = document.createElement("div");
  const dailyKicker = document.createElement("span");
  dailyKicker.className = "section-kicker";
  dailyKicker.textContent = "历史与预测";
  const dailyTitle = document.createElement("strong");
  dailyTitle.textContent = "每日最高 / 最低气温";
  dailyHeading.append(dailyKicker, dailyTitle);
  const dailySource = document.createElement("span");
  dailySource.className = "daily-temperature-source";
  dailySource.setAttribute("aria-live", "polite");
  dailySource.textContent = "DB 历史 · 最新预报";
  dailyHeader.append(dailyHeading, dailySource);
  const dailyChart = document.createElement("div");
  dailyChart.className = "daily-temperature-chart";
  dailyChart.setAttribute("aria-busy", "true");
  const chartPlaceholder = document.createElement("div");
  chartPlaceholder.className = "chart-placeholder";
  chartPlaceholder.textContent = "—";
  dailyChart.append(chartPlaceholder);
  dailyTemperature.append(dailyHeader, dailyChart);
  target.append(climate, radar, dailyTemperature);

  const stationName = weather?.station?.name || selectedStation;
  if (stationName) {
    let loadedPoints: DailyTemperaturePoint[] = [];
    let nextBeforeDate: string | null = null;
    let hasMoreHistory = false;
    let historyPageLoading = false;

    const requestHistoryPage = async (
      beforeDate: string | null,
      preserveInterval?: number,
    ): Promise<void> => {
      if (historyPageLoading) return;
      historyPageLoading = true;
      dailySource.textContent = beforeDate ? "正在加载更早历史…" : "正在读取历史…";
      try {
        const response = await invoke<TemperatureHistoryResponse>("get_temperature_history", {
          stationName,
          unifiedUuid: weather?.station?.unified_uuid || null,
          beforeDate,
          pageSize: beforeDate
            ? DAILY_TEMPERATURE_NEXT_PAGE_SIZE
            : DAILY_TEMPERATURE_INITIAL_PAGE_SIZE,
        });
        if (temperatureRequestToken !== dailyTemperatureRequestToken
          || stationName !== selectedStation
          || !dailyChart.isConnected) return;
        const previousFirstDate = loadedPoints[0]?.date;
        const mergedPoints = mergeDailyTemperaturePoints(loadedPoints, response.points);
        const prependedPoints = previousFirstDate
          ? Math.max(0, mergedPoints.findIndex((point) => point.date === previousFirstDate))
          : 0;
        loadedPoints = mergedPoints;
        nextBeforeDate = response.next_before_date?.trim() || null;
        hasMoreHistory = response.has_more_history && nextBeforeDate !== null;
        dailySource.textContent = hasMoreHistory
          ? "DB 历史分段加载 · 最新预报"
          : "DB 全部历史 · 最新预报";
        renderDailyTemperatureChart(dailyChart, loadedPoints, {
          initialInterval: preserveInterval === undefined
            ? undefined
            : preserveInterval + prependedPoints,
          onReachPast: () => {
            if (!historyPageLoading && hasMoreHistory && nextBeforeDate) {
              void requestHistoryPage(
                nextBeforeDate,
                dailyTemperatureScrollInterval(dailyChart),
              );
            }
          },
        });
      } catch (error) {
        if (temperatureRequestToken !== dailyTemperatureRequestToken
          || stationName !== selectedStation
          || !dailyChart.isConnected) return;
        if (!loadedPoints.length) {
          dailyChart.replaceChildren(emptyCopy(`气温历史读取失败：${errorMessage(error)}`));
          dailyChart.setAttribute("aria-busy", "false");
        } else {
          dailySource.textContent = "历史分段加载失败 · 可重试滑动";
          toast(`更早气温历史加载失败：${errorMessage(error)}`, "error");
        }
      } finally {
        historyPageLoading = false;
      }
    };

    void requestHistoryPage(null);
  } else {
    dailyChart.replaceChildren(emptyCopy("暂无历史/预报气温数据"));
    dailyChart.setAttribute("aria-busy", "false");
  }
  const resourceId = placeholder ? "" : weather?.radar?.image_resource_id?.trim() ?? "";
  if (resourceId) {
    image.dataset.resourceId = resourceId;
    void engineResourceUrl(resourceId).then((url) => {
      if (renderToken === radarRenderToken && image.isConnected && image.dataset.resourceId === resourceId) {
        image.addEventListener("load", () => {
          if (renderToken !== radarRenderToken
            || !image.isConnected
            || image.dataset.resourceId !== resourceId
            || image.currentSrc !== url) return;
          const viewerTitle = weather?.radar?.title?.trim() || `${weather?.station?.name ?? "当前站点"}雷达图`;
          image.alt = viewerTitle;
          enableImageViewer(radar, url, viewerTitle, resourceId);
        }, { once: true });
        image.src = url;
      }
    }).catch((error) => {
      if (renderToken === radarRenderToken && image.isConnected) {
        title.textContent = "雷达图加载失败";
        log(`雷达图加载失败：${errorMessage(error)}`, "error");
      }
    });
  }
}

async function engineResourceUrl(resourceId: string): Promise<string> {
  const cached = resourceUrls.get(resourceId);
  if (cached) {
    resourceUrls.delete(resourceId);
    resourceUrls.set(resourceId, cached);
    return cached;
  }
  const pending = resourceRequests.get(resourceId);
  if (pending) return pending;
  const request = invokeBinaryCommand("get_resource_bytes", { resourceId }).then((buffer) => {
    const url = URL.createObjectURL(new Blob([buffer], { type: imageResourceMimeType(buffer) }));
    resourceUrls.set(resourceId, url);
    while (resourceUrls.size > RESOURCE_URL_CACHE_SIZE) {
      const oldest = [...resourceUrls.entries()].find(([candidate]) => candidate !== imageViewerResourceId);
      if (!oldest) break;
      resourceUrls.delete(oldest[0]);
      URL.revokeObjectURL(oldest[1]);
    }
    return url;
  }).finally(() => {
    resourceRequests.delete(resourceId);
  });
  resourceRequests.set(resourceId, request);
  return request;
}

function imageResourceMimeType(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer, 0, Math.min(buffer.byteLength, 256));
  if (bytes.length >= 8
    && bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47
    && bytes[4] === 0x0d && bytes[5] === 0x0a && bytes[6] === 0x1a && bytes[7] === 0x0a) return "image/png";
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) return "image/jpeg";
  if (bytes.length >= 6) {
    const signature = String.fromCharCode(...bytes.slice(0, 6));
    if (signature === "GIF87a" || signature === "GIF89a") return "image/gif";
  }
  if (bytes.length >= 12
    && String.fromCharCode(...bytes.slice(0, 4)) === "RIFF"
    && String.fromCharCode(...bytes.slice(8, 12)) === "WEBP") return "image/webp";
  const textPrefix = new TextDecoder().decode(bytes).trimStart().toLocaleLowerCase();
  if (textPrefix.startsWith("<svg") || (textPrefix.startsWith("<?xml") && textPrefix.includes("<svg"))) {
    return "image/svg+xml";
  }
  return "application/octet-stream";
}

function emptyCopy(message: string): HTMLParagraphElement {
  const node = document.createElement("p");
  node.className = "muted empty-copy";
  node.textContent = message;
  return node;
}

function renderManageStations(previousLayout: LayoutSnapshot | null = null): void {
  manageList.replaceChildren();
  if (!config.stations.length) {
    manageList.append(emptyCopy("尚未配置站点"));
    return;
  }
  config.stations.forEach((station, index) => {
    const row = document.createElement("div");
    row.className = "manage-row";
    row.dataset.motionKey = station.name;
    const order = document.createElement("span");
    order.className = "manage-order";
    order.textContent = String(index + 1).padStart(2, "0");
    const copy = document.createElement("div");
    const title = document.createElement("strong");
    title.textContent = station.name;
    const meta = document.createElement("small");
    meta.textContent = `${station.enabled ? "已启用" : "已停用"}${hiddenStations.has(station.name) ? " · GUI 已隐藏" : ""}`;
    copy.append(title, meta);
    const actions = document.createElement("div");
    actions.className = "manage-actions";
    actions.append(
      manageAction(station.enabled ? "停用" : "启用", (event) => {
        void toggleStation(index, shouldAnimateInteraction(event));
      }),
      manageAction(hiddenStations.has(station.name) ? "显示" : "隐藏", (event) => {
        toggleHidden(station.name, shouldAnimateInteraction(event));
      }),
      manageIconAction("chevron-up", "上移", (event) => {
        void moveStation(index, -1, shouldAnimateInteraction(event));
      }, index === 0),
      manageIconAction("chevron-down", "下移", (event) => {
        void moveStation(index, 1, shouldAnimateInteraction(event));
      }, index === config.stations.length - 1),
      manageAction("删除", (event) => {
        void removeStation(index, shouldAnimateInteraction(event));
      }, false, "danger"),
    );
    row.append(order, copy, actions);
    manageList.append(row);
  });
  animateLayoutFrom(manageList, ".manage-row", previousLayout);
}

function manageAction(
  label: string,
  action: (event: MouseEvent) => void,
  disabled = false,
  variant = "",
): HTMLButtonElement {
  const button = document.createElement("button");
  button.className = `text-button ${variant}`;
  button.textContent = label;
  button.disabled = disabled;
  button.addEventListener("click", action);
  return button;
}

function manageIconAction(
  icon: Extract<UiIconName, "chevron-up" | "chevron-down">,
  label: string,
  action: (event: MouseEvent) => void,
  disabled = false,
): HTMLButtonElement {
  const button = manageAction(label, action, disabled);
  button.classList.add("icon-only");
  button.replaceChildren(uiIcon(icon));
  button.setAttribute("aria-label", label);
  button.title = label;
  return button;
}

async function submitStations(
  stations: StationConfig[],
  success: string,
  previousLayout: LayoutSnapshot | null = null,
  animateWeather = true,
): Promise<void> {
  try {
    config = await invoke<AppConfig>("update_stations", { stations });
    normalizeSelection();
    renderStations();
    renderManageStations(previousLayout);
    toast(success, "success", animateWeather);
    log(success);
    void loadStationSummaries();
    if (!snapshot || snapshot.station?.name !== selectedStation) {
      await loadWeather(false, animateWeather);
    }
  } catch (error) {
    const message = errorMessage(error);
    toast(`配置更新失败：${message}`, "error", animateWeather);
    log(`配置更新失败：${message}`, "error");
  }
}

async function toggleStation(index: number, animate = true): Promise<void> {
  const stations = config.stations.map((station) => ({ ...station }));
  const target = stations[index];
  if (!target) return;
  target.enabled = !target.enabled;
  await submitStations(
    stations,
    `${target.enabled ? "已启用" : "已停用"} ${target.name}`,
    null,
    animate,
  );
  if (animate) {
    const updated = [...manageList.querySelectorAll<HTMLElement>(".manage-row")]
      .find((row) => row.dataset.motionKey === target.name);
    if (updated) animateStateChange(updated);
  }
}

function toggleHidden(name: string, animate = true): void {
  hiddenStations.has(name) ? hiddenStations.delete(name) : hiddenStations.add(name);
  normalizeSelection();
  renderStations();
  renderManageStations();
  if (animate) animateStateChange(stationList);
  void loadStationSummaries();
  if (selectedStation) void loadWeather(false, animate);
}

async function moveStation(index: number, delta: number, animate = true): Promise<void> {
  const destination = index + delta;
  if (destination < 0 || destination >= config.stations.length) return;
  const stations = config.stations.map((station) => ({ ...station }));
  const [moved] = stations.splice(index, 1);
  if (!moved) return;
  stations.splice(destination, 0, moved);
  const previousLayout = animate ? captureLayout(manageList, ".manage-row") : null;
  await submitStations(stations, `已移动 ${moved.name}`, previousLayout, animate);
}

async function removeStation(index: number, animate = true): Promise<void> {
  const target = config.stations[index];
  if (!target || !window.confirm(`确定删除 ${target.name}？`)) return;
  const stations = config.stations.filter((_, stationIndex) => stationIndex !== index);
  hiddenStations.delete(target.name);
  const previousLayout = animate ? captureLayout(manageList, ".manage-row") : null;
  await submitStations(stations, `已删除 ${target.name}`, previousLayout, animate);
}

function showDialog(dialog: HTMLDialogElement, animate = true): void {
  if (dialog.open) return;
  dialog.dataset.motionState = animate && !shouldReduceMotion() ? "opening" : "instant";
  dialog.showModal();
  if (animate) animateDialogIn(dialog);
  if (animate && !shouldReduceMotion()) {
    window.requestAnimationFrame(() => {
      if (dialog.open) dialog.dataset.motionState = "open";
    });
  }
}

async function hideDialog(dialog: HTMLDialogElement, animate = true): Promise<void> {
  if (!dialog.open) return;
  if (animate && !shouldReduceMotion()) {
    dialog.dataset.motionState = "closing";
    await waitForMotion(animateDialogOut(dialog));
  }
  if (dialog.open) dialog.close();
  delete dialog.dataset.motionState;
}

async function openSearch(animate = true): Promise<void> {
  if (manageDialog.open) await hideDialog(manageDialog, animate);
  resetSearch(true);
  showDialog(searchDialog, animate);
  window.setTimeout(() => element<HTMLInputElement>("search-input").focus(), 50);
}

function resetSearch(clearInput = false): void {
  if (searchDebounceTimer !== undefined) window.clearTimeout(searchDebounceTimer);
  searchDebounceTimer = undefined;
  searchRequestToken += 1;
  searchResults = [];
  selectedSearchResult = null;
  if (clearInput) element<HTMLInputElement>("search-input").value = "";
  element("search-summary").textContent = "输入关键词开始搜索";
  const results = element<HTMLDivElement>("search-results");
  results.replaceChildren();
  results.removeAttribute("aria-busy");
  renderPreview(null, null);
}

function scheduleSearch(query: string): void {
  if (searchDebounceTimer !== undefined) window.clearTimeout(searchDebounceTimer);
  const token = ++searchRequestToken;
  selectedSearchResult = null;
  renderPreview(null, null);
  if (!query) {
    searchResults = [];
    const results = element<HTMLDivElement>("search-results");
    results.replaceChildren();
    results.removeAttribute("aria-busy");
    element("search-summary").textContent = "输入关键词开始搜索";
    return;
  }
  element("search-summary").textContent = "正在等待输入完成…";
  searchDebounceTimer = window.setTimeout(() => {
    searchDebounceTimer = undefined;
    void runSearch(query, token);
  }, SEARCH_DEBOUNCE_MS);
}

async function runSearch(query: string, token = ++searchRequestToken): Promise<void> {
  const summary = element("search-summary");
  const results = element<HTMLDivElement>("search-results");
  summary.textContent = "正在搜索…";
  results.setAttribute("aria-busy", "true");
  selectedSearchResult = null;
  renderPreview(null, null);
  try {
    const response = await invoke<StationRef[]>("search_stations", { query });
    if (token !== searchRequestToken) return;
    searchResults = response;
    summary.textContent = `找到 ${searchResults.length} 个站点`;
    renderSearchResults();
  } catch (error) {
    if (token !== searchRequestToken) return;
    summary.textContent = `搜索失败：${errorMessage(error)}`;
  } finally {
    if (token === searchRequestToken) results.removeAttribute("aria-busy");
  }
}

function renderSearchResults(): void {
  const target = element<HTMLDivElement>("search-results");
  target.replaceChildren();
  if (!searchResults.length) {
    target.append(emptyCopy("没有匹配结果，请尝试更短的关键词"));
    return;
  }
  for (const station of searchResults) {
    const button = document.createElement("button");
    button.className = "search-result";
    button.dataset.active = String(selectedSearchResult?.unified_uuid === station.unified_uuid);
    const copy = document.createElement("span");
    const title = document.createElement("strong");
    title.textContent = station.name;
    const path = document.createElement("small");
    path.textContent = [station.province, station.city].filter(Boolean).join(" · ");
    copy.append(title, path);
    const arrow = uiIcon("chevron-right", "search-result-arrow");
    button.append(copy, arrow);
    button.addEventListener("click", (event) => {
      void previewStation(station, shouldAnimateInteraction(event));
    });
    target.append(button);
  }
}

async function previewStation(station: StationRef, animate = true): Promise<void> {
  selectedSearchResult = station;
  renderSearchResults();
  renderPreview(station, null, true, "", animate);
  try {
    const weather = await invoke<WeatherSnapshot>("get_weather", {
      stationName: station.name,
      unifiedUuid: station.unified_uuid || null,
      refresh: false,
    });
    if (selectedSearchResult?.name === station.name) renderPreview(station, weather, false, "", animate);
  } catch (error) {
    renderPreview(station, null, false, errorMessage(error), animate);
  }
}

function renderPreview(
  station: StationRef | null,
  weather: WeatherSnapshot | null,
  loading = false,
  error = "",
  animate = false,
): void {
  const panel = element<HTMLElement>("preview-panel");
  panel.replaceChildren();
  const image = document.createElement("img");
  const description = currentWeatherDescription(weather);
  image.src = station ? weatherAsset(description) : assets.emptyStations;
  image.alt = "天气预览";
  panel.append(image);
  if (!station) {
    panel.append(emptyCopy("选择结果可预览天气"));
    return;
  }
  const title = document.createElement("h3");
  title.textContent = station.name;
  const detail = document.createElement("p");
  detail.textContent = loading ? "正在加载天气…" : error ? `预览失败：${error}` : `${value(weather?.real?.temperature, "°", 1)} · ${text(description)}`;
  const add = document.createElement("button");
  add.className = "button primary";
  add.textContent = config.stations.some((item) => item.name === station.name) ? "启用此站点" : "添加到我的城市";
  add.addEventListener("click", (event) => {
    void addStation(station, shouldAnimateInteraction(event));
  });
  panel.append(title, detail, add);
  if (animate) animateStateChange(panel);
}

async function addStation(station: StationRef, animate = true): Promise<void> {
  const stations = config.stations.map((item) => ({ ...item }));
  const existing = stations.find((item) => item.name === station.name);
  if (existing) existing.enabled = true;
  else stations.push({ name: station.name, enabled: true });
  await submitStations(
    stations,
    `${existing ? "已启用" : "已添加"} ${station.name}`,
    null,
    animate,
  );
  selectedStation = station.name;
  await hideDialog(searchDialog, animate);
  await loadWeather(false, animate);
}

function renderEngine(): void {
  element("engine-mode").textContent = engine ? `${engine.mode || "local"} · schema ${engine.schema_version || "—"}` : "本地天气服务";
  const target = element<HTMLDivElement>("engine-details");
  target.replaceChildren();
  if (!engine) {
    target.append(emptyCopy("暂无引擎状态"));
    return;
  }
  const entries: Array<[string, string]> = [
    ["运行状态", engine.ready ? "就绪" : "未就绪"],
    ["运行模式", engine.mode || "—"],
    ["引擎版本", engine.engine_version || engine.build_version || "—"],
    ["协议版本", engine.schema_version || "—"],
    ["配置路径", engine.config_path || "—"],
    ["RPC", engine.rpc_endpoint || "—"],
  ];
  for (const [label, reading] of entries) {
    const row = document.createElement("div");
    const key = document.createElement("span");
    key.textContent = label;
    const valueNode = document.createElement("strong");
    valueNode.textContent = reading;
    row.append(key, valueNode);
    target.append(row);
  }
}

async function refreshEngineStatus(animate = true): Promise<void> {
  try {
    engine = await invoke<EngineStatus>("engine_status");
    renderEngine();
    if (animate) animateStateChange(element("engine-details"));
    toast("引擎状态已刷新", "success", animate);
  } catch (error) {
    toast(`状态刷新失败：${errorMessage(error)}`, "error", animate);
  }
}

async function showConfig(defaults: boolean, animate = true): Promise<void> {
  const output = element<HTMLPreElement>("config-output");
  const wasHidden = output.hidden;
  output.hidden = false;
  output.textContent = "正在读取…";
  if (animate && wasHidden) animateEntrance(output, 0, 5);
  try {
    output.textContent = await invoke<string>("get_config_text", { defaults });
    if (animate) animateStateChange(output);
  } catch (error) {
    output.textContent = `读取失败：${errorMessage(error)}`;
  }
}

async function engineAction(command: "restart_engine" | "stop_engine", animate = true): Promise<void> {
  const label = command === "restart_engine" ? "重启" : "停止";
  if (!window.confirm(`确定${label}天气引擎？`)) return;
  try {
    const message = await invoke<string>(command);
    toast(message, "success", animate);
    log(message);
    if (command === "stop_engine") setConnection("failed", animate);
  } catch (error) {
    toast(`${label}失败：${errorMessage(error)}`, "error", animate);
  }
}

function bindEvents(): void {
  bootRetry.addEventListener("click", () => void initialize());
  element("refresh-button").addEventListener("click", (event) => {
    void loadWeather(true, shouldAnimateInteraction(event));
  });
  ["search-button", "sidebar-add", "empty-add", "manage-search"].forEach((id) => {
    element(id).addEventListener("click", (event) => {
      void openSearch(shouldAnimateInteraction(event));
    });
  });
  element("manage-button").addEventListener("click", (event) => {
    showDialog(manageDialog, shouldAnimateInteraction(event));
  });
  element("about-button").addEventListener("click", (event) => {
    showDialog(aboutDialog, shouldAnimateInteraction(event));
  });
  runtimeToggle.addEventListener("click", (event) => {
    setRuntimePanel(true, false, shouldAnimateInteraction(event));
  });
  element("runtime-close").addEventListener("click", (event) => {
    setRuntimePanel(false, true, shouldAnimateInteraction(event));
  });
  document.addEventListener("click", (event) => {
    if (runtimePanel.dataset.open !== "true") return;
    const path = event.composedPath();
    if (path.includes(runtimePanel) || path.includes(runtimeToggle)) return;
    setRuntimePanel(false, false, true);
  });
  element("clear-log").addEventListener("click", () => { activity.length = 0; renderLog(false); });
  element("status-refresh").addEventListener("click", (event) => {
    void refreshEngineStatus(shouldAnimateInteraction(event));
  });
  element("show-config").addEventListener("click", (event) => {
    void showConfig(false, shouldAnimateInteraction(event));
  });
  element("show-defaults").addEventListener("click", (event) => {
    void showConfig(true, shouldAnimateInteraction(event));
  });
  element("restart-engine").addEventListener("click", (event) => {
    void engineAction("restart_engine", shouldAnimateInteraction(event));
  });
  element("stop-engine").addEventListener("click", (event) => {
    void engineAction("stop_engine", shouldAnimateInteraction(event));
  });
  element("data-update-retry").addEventListener("click", (event) => {
    void loadWeather(true, shouldAnimateInteraction(event));
  });
  const debugMode = element<HTMLInputElement>("debug-mode");
  debugMode.addEventListener("click", (event) => {
    debugMode.dataset.animate = String(shouldAnimateInteraction(event));
  });
  debugMode.addEventListener("change", () => {
    void updateGuiDebug(debugMode.checked, debugMode.dataset.animate !== "false");
  });
  element("theme-button").addEventListener("click", (event) => {
    toggleTheme(shouldAnimateInteraction(event));
  });
  element<HTMLFormElement>("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const query = element<HTMLInputElement>("search-input").value.trim();
    if (!query) return;
    if (searchDebounceTimer !== undefined) window.clearTimeout(searchDebounceTimer);
    searchDebounceTimer = undefined;
    void runSearch(query, ++searchRequestToken);
  });
  element<HTMLInputElement>("search-input").addEventListener("input", (event) => {
    scheduleSearch((event.currentTarget as HTMLInputElement).value.trim());
  });
  searchDialog.addEventListener("close", () => resetSearch(false));
  document.querySelectorAll<HTMLElement>("[data-close]").forEach((button) => {
    button.addEventListener("click", (event) => {
      void hideDialog(
        element<HTMLDialogElement>(button.dataset.close!),
        shouldAnimateInteraction(event),
      );
    });
  });
  document.querySelectorAll<HTMLDialogElement>("dialog").forEach((dialog) => {
    dialog.addEventListener("click", (event) => {
      if (event.target === dialog) void hideDialog(dialog, true);
    });
    dialog.addEventListener("cancel", (event) => {
      event.preventDefault();
      void hideDialog(dialog, false);
    });
  });
  bindImageViewerEvents();
  document.addEventListener("contextmenu", (event) => {
    if (guiConfig.debug) return;
    event.preventDefault();
    event.stopImmediatePropagation();
  }, { capture: true });
  document.addEventListener("keydown", (event) => {
    if (isDevtoolsShortcut(event)) {
      event.preventDefault();
      event.stopImmediatePropagation();
      if (guiConfig.debug) openGuiDevtools();
      return;
    }
    if (imageViewerOpen()) {
      if (event.key === "Escape") {
        event.preventDefault();
        void closeImageViewer(true, false);
      } else if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        setImageViewerScale(imageViewerScale + IMAGE_VIEWER_SCALE_STEP, false);
      } else if (event.key === "-" || event.key === "_") {
        event.preventDefault();
        setImageViewerScale(imageViewerScale - IMAGE_VIEWER_SCALE_STEP, false);
      } else if (event.key === "0") {
        event.preventDefault();
        resetImageViewer(false);
      }
      return;
    }
    if (event.key === "Escape" && runtimePanel.dataset.open === "true") {
      setRuntimePanel(false, true, false);
    }
  });
}

function bindImageViewerEvents(): void {
  const dragHandle = element<HTMLElement>("image-viewer-drag-handle");
  let panelDrag: { pointerId: number; startX: number; startY: number; left: number; top: number } | null = null;
  let imageDrag: { pointerId: number; startX: number; startY: number; panX: number; panY: number } | null = null;

  element("image-viewer-close").addEventListener("click", (event) => {
    void closeImageViewer(true, shouldAnimateInteraction(event));
  });
  element("image-viewer-zoom-in").addEventListener("click", (event) => {
    setImageViewerScale(imageViewerScale + IMAGE_VIEWER_SCALE_STEP, shouldAnimateInteraction(event));
  });
  element("image-viewer-zoom-out").addEventListener("click", (event) => {
    setImageViewerScale(imageViewerScale - IMAGE_VIEWER_SCALE_STEP, shouldAnimateInteraction(event));
  });
  element("image-viewer-reset").addEventListener("click", (event) => {
    resetImageViewer(shouldAnimateInteraction(event));
  });
  imageViewer.addEventListener("click", (event) => {
    if (event.target === imageViewer) void closeImageViewer(true, true);
  });
  imageViewerViewport.addEventListener("wheel", (event) => {
    event.preventDefault();
    setImageViewerScale(imageViewerScale + (event.deltaY < 0 ? IMAGE_VIEWER_SCALE_STEP : -IMAGE_VIEWER_SCALE_STEP));
  }, { passive: false });
  imageViewerViewport.addEventListener("dblclick", () => {
    setImageViewerScale(imageViewerScale === 1 ? 2 : 1);
    if (imageViewerScale === 1) {
      imageViewerPanX = 0;
      imageViewerPanY = 0;
      updateImageViewerTransform();
    }
  });

  dragHandle.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || (event.target as Element).closest("button")) return;
    const bounds = imageViewerPanel.getBoundingClientRect();
    panelDrag = { pointerId: event.pointerId, startX: event.clientX, startY: event.clientY, left: bounds.left, top: bounds.top };
    dragHandle.setPointerCapture(event.pointerId);
    dragHandle.dataset.dragging = "true";
    event.preventDefault();
  });
  dragHandle.addEventListener("pointermove", (event) => {
    if (!panelDrag || panelDrag.pointerId !== event.pointerId) return;
    imageViewerPanel.style.left = `${panelDrag.left + event.clientX - panelDrag.startX}px`;
    imageViewerPanel.style.top = `${panelDrag.top + event.clientY - panelDrag.startY}px`;
    constrainImageViewerPanel();
  });
  const finishPanelDrag = (event: PointerEvent): void => {
    if (!panelDrag || panelDrag.pointerId !== event.pointerId) return;
    panelDrag = null;
    delete dragHandle.dataset.dragging;
    if (dragHandle.hasPointerCapture(event.pointerId)) dragHandle.releasePointerCapture(event.pointerId);
  };
  dragHandle.addEventListener("pointerup", finishPanelDrag);
  dragHandle.addEventListener("pointercancel", finishPanelDrag);

  imageViewerViewport.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || imageViewerViewport.dataset.pannable !== "true") return;
    imageDrag = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      panX: imageViewerPanX,
      panY: imageViewerPanY,
    };
    imageViewerViewport.setPointerCapture(event.pointerId);
    imageViewerViewport.dataset.dragging = "true";
    event.preventDefault();
  });
  imageViewerViewport.addEventListener("pointermove", (event) => {
    if (!imageDrag || imageDrag.pointerId !== event.pointerId) return;
    imageViewerPanX = imageDrag.panX + event.clientX - imageDrag.startX;
    imageViewerPanY = imageDrag.panY + event.clientY - imageDrag.startY;
    updateImageViewerTransform();
  });
  const finishImageDrag = (event: PointerEvent): void => {
    if (!imageDrag || imageDrag.pointerId !== event.pointerId) return;
    imageDrag = null;
    delete imageViewerViewport.dataset.dragging;
    if (imageViewerViewport.hasPointerCapture(event.pointerId)) imageViewerViewport.releasePointerCapture(event.pointerId);
  };
  imageViewerViewport.addEventListener("pointerup", finishImageDrag);
  imageViewerViewport.addEventListener("pointercancel", finishImageDrag);
  window.addEventListener("resize", () => {
    constrainImageViewerPanel();
    updateImageViewerTransform(false);
  });
}

function toggleTheme(animate = true): void {
  const current = document.documentElement.dataset.theme;
  const next = current === "dark" ? "light" : current === "light" ? "system" : "dark";
  const applyTheme = (): void => {
    if (next === "system") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = next;
    localStorage.setItem("weather-theme", next);
  };
  const viewTransitionDocument = document as Document & {
    startViewTransition?: (update: () => void) => unknown;
  };
  if (animate && !shouldReduceMotion() && viewTransitionDocument.startViewTransition) {
    viewTransitionDocument.startViewTransition(applyTheme);
  } else {
    applyTheme();
    if (animate) {
      playMotion(appElement, [{ opacity: 0.76 }, { opacity: 1 }], {
        duration: motionDuration.standard,
        easing: motionEasing.out,
      });
    }
  }
  toast(`主题：${next === "system" ? "跟随系统" : next === "dark" ? "深色" : "浅色"}`, "info", animate);
}

function restoreTheme(): void {
  const theme = localStorage.getItem("weather-theme");
  if (theme === "dark" || theme === "light") document.documentElement.dataset.theme = theme;
}

async function bindEngineEvents(): Promise<void> {
  await listen<void>("gui-close-requested", () => {
    if (appExitStarted) return;
    appExitStarted = true;
    animateApplicationExit(document.body);
  });
  await listen<string>("connection-status", (event) => setConnection(event.payload as "connecting" | "connected" | "failed"));
  await listen<GuiEngineEvent>("engine-event", (message) => {
    const event = message.payload;
    if (event.type === "weather") {
      const incoming = event.snapshot.station?.name;
      if (incoming) {
        weatherCache.set(incoming, event.snapshot);
        loadedStationSummaries.add(incoming);
        renderStations();
      }
      if (incoming && incoming === selectedStation) {
        snapshot = event.snapshot;
        renderWeather(snapshot);
        if (event.snapshot.stale) {
          showDataUpdateFailure("上游更新未成功，已回退到缓存");
        } else {
          clearDataUpdateFailure();
        }
      }
      return;
    }
    if (event.type === "status") {
      engine = event.status;
      renderEngine();
      log(`引擎状态：${event.status.ready ? "就绪" : "未就绪"}`);
      return;
    }
    if (event.type === "fetch") {
      const ok = event.event.ok === true;
      log(`${event.topic} · ${String(event.event.endpoint ?? "数据请求")} · ${ok ? "完成" : "失败"}`, ok ? "info" : "error");
      return;
    }
    if (event.type === "refresh") {
      const completed = event.event.completed === true;
      log(`${event.topic} · ${completed ? "刷新完成" : "开始刷新"}`);
      const eventUuid = String(event.event.unified_uuid ?? "");
      const selectedUuid = snapshot?.station?.unified_uuid ?? "";
      // Before the first snapshot arrives, the scheduler may emit refresh
      // outcomes for any configured station. The foreground get_weather call
      // owns startup error reporting; only unscoped events may apply here.
      const appliesToSelection = selectedUuid
        ? !eventUuid || eventUuid === selectedUuid
        : !eventUuid;
      const outcome = Number(event.event.outcome ?? 0);
      if (completed && appliesToSelection && (outcome === 2 || outcome === 3)) {
        showDataUpdateFailure(String(event.event.message ?? "未能取得最新天气数据"));
      }
      return;
    }
    log(event.message, event.level);
  });
}

restoreTheme();
document.documentElement.dataset.debug = "false";
bindEvents();
renderLog();
void Promise.all([initializeGuiConfig(), bindEngineEvents()]).then(initialize);
