export const assets = {
  logo: new URL("../assets/brand/logo.svg", import.meta.url).href,
  emptyStations: new URL("../assets/illustrations/empty-stations.svg", import.meta.url).href,
  radarPlaceholder: new URL("../assets/illustrations/radar-placeholder.svg", import.meta.url).href,
  radarDemo: new URL("../assets/illustrations/radar-demo.svg", import.meta.url).href,
  icons: {
    comfort: new URL("../assets/icons/comfort.svg", import.meta.url).href,
    comfortIndex: new URL("../assets/icons/comfort-index.svg", import.meta.url).href,
  },
  weather: {
    clear: new URL("../assets/weather/clear.svg", import.meta.url).href,
    cloudy: new URL("../assets/weather/cloudy.svg", import.meta.url).href,
    rain: new URL("../assets/weather/rain.svg", import.meta.url).href,
    storm: new URL("../assets/weather/storm.svg", import.meta.url).href,
    snow: new URL("../assets/weather/snow.svg", import.meta.url).href,
    fog: new URL("../assets/weather/fog.svg", import.meta.url).href,
    unknown: new URL("../assets/weather/unknown.svg", import.meta.url).href,
  },
} as const;

export function usableWeatherDescription(
  ...candidates: Array<string | null | undefined>
): string | null {
  for (const candidate of candidates) {
    const value = candidate?.trim();
    if (value && !["-", "--", "—"].includes(value)) return value;
  }
  return null;
}

export function weatherAsset(description?: string | null): string {
  const value = (description ?? "").toLowerCase();
  if (/雷|storm|thunder/.test(value)) return assets.weather.storm;
  if (/雪|snow|sleet|冰雹/.test(value)) return assets.weather.snow;
  if (/雨|rain|shower|drizzle/.test(value)) return assets.weather.rain;
  if (/雾|霾|fog|haze|mist|沙尘/.test(value)) return assets.weather.fog;
  if (/云|阴|cloud|overcast/.test(value)) return assets.weather.cloudy;
  if (/晴|clear|sunny/.test(value)) return assets.weather.clear;
  return assets.weather.unknown;
}
