import { describe, expect, test } from "bun:test";

import { pollutantReadings } from "../src/air";

describe("pollutantReadings", () => {
  test("preserves every pollutant slot and marks unavailable values as null", () => {
    expect(pollutantReadings({
      aqi: 46,
      pm2_5: 18,
      co: 0.7,
      no2: null,
    })).toEqual([
      { label: "PM2.5", reading: 18, unit: "µg/m³" },
      { label: "PM10", reading: null, unit: "µg/m³" },
      { label: "NO₂", reading: null, unit: "µg/m³" },
      { label: "SO₂", reading: null, unit: "µg/m³" },
      { label: "CO", reading: 0.7, unit: "mg/m³" },
      { label: "O₃", reading: null, unit: "µg/m³" },
    ]);
  });

  test("does not invent component values from the overall AQI", () => {
    expect(pollutantReadings({ aqi: 46, category: "优" }).every(({ reading }) => reading === null)).toBe(true);
  });

  test("filters non-finite component values", () => {
    const readings = pollutantReadings({ pm10: Number.NaN, o3: Number.POSITIVE_INFINITY });
    expect(readings.every(({ reading }) => reading === null)).toBe(true);
  });
});
