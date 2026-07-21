import type { AirQuality } from "./types";

export interface PollutantReading {
  label: string;
  reading: number | null;
  unit: "µg/m³" | "mg/m³";
}

const pollutantFields = [
  ["PM2.5", "pm2_5", "µg/m³"],
  ["PM10", "pm10", "µg/m³"],
  ["NO₂", "no2", "µg/m³"],
  ["SO₂", "so2", "µg/m³"],
  ["CO", "co", "mg/m³"],
  ["O₃", "o3", "µg/m³"],
] as const satisfies ReadonlyArray<readonly [string, keyof AirQuality, PollutantReading["unit"]]>;

export function pollutantReadings(
  air: AirQuality | null | undefined,
): PollutantReading[] {
  return pollutantFields.map(([label, field, unit]) => {
    const reading = air?.[field];
    return {
      label,
      reading: typeof reading === "number" && Number.isFinite(reading) ? reading : null,
      unit,
    };
  });
}
