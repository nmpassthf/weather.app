import { describe, expect, test } from "bun:test";

import {
  recentTemperatureHistoryRows,
  savitzkyGolayTemperaturePlotSamples,
  temperatureHistoryTickIndices,
} from "../src/history";
import type { PassedWeatherChart } from "../src/types";

function row(time: string, temperature: number): PassedWeatherChart {
  return { time, temperature };
}

function hourlyRowsNewestFirst(count: number): PassedWeatherChart[] {
  const newest = Date.UTC(2026, 6, 20, 10);
  return Array.from({ length: count }, (_, index) => {
    const date = new Date(newest - index * 60 * 60 * 1_000);
    return row(`${date.toISOString().slice(0, 10)} ${date.toISOString().slice(11, 16)}`, index);
  });
}

describe("recentTemperatureHistoryRows", () => {
  test("orders NMC newest-first rows from old to new and keeps the latest window", () => {
    const source = hourlyRowsNewestFirst(24);

    const result = recentTemperatureHistoryRows(source, 18);

    expect(result).toHaveLength(18);
    expect(result[0]?.time).toBe("2026-07-19 17:00");
    expect(result.at(-1)?.time).toBe("2026-07-20 10:00");
    expect(result.map(({ temperature }) => temperature)).toEqual([
      17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
    ]);
    expect(source[0]?.time).toBe("2026-07-20 10:00");
  });

  test("also accepts already chronological rows", () => {
    const source = hourlyRowsNewestFirst(4).reverse();

    expect(recentTemperatureHistoryRows(source, 3).map(({ time }) => time)).toEqual([
      "2026-07-20 08:00",
      "2026-07-20 09:00",
      "2026-07-20 10:00",
    ]);
  });

  test("falls back to the NMC newest-first contract when dates are absent", () => {
    const source = [row("10:00", 10), row("09:00", 9), row("08:00", 8), row("07:00", 7)];

    expect(recentTemperatureHistoryRows(source, 3).map(({ time }) => time)).toEqual([
      "08:00",
      "09:00",
      "10:00",
    ]);
  });

  test("ignores non-finite and missing temperatures", () => {
    const source = [
      row("2026-07-20 10:00", Number.NaN),
      { time: "2026-07-20 09:00" },
      row("2026-07-20 08:00", 28),
    ];

    expect(recentTemperatureHistoryRows(source)).toEqual([row("2026-07-20 08:00", 28)]);
  });
});

describe("temperatureHistoryTickIndices", () => {
  test("uses six evenly spaced ticks for the 18-point history window", () => {
    expect(temperatureHistoryTickIndices(18)).toEqual([0, 3, 6, 9, 12, 15]);
  });

  test("uses three ticks for a shorter series without duplicating points", () => {
    expect(temperatureHistoryTickIndices(8)).toEqual([0, 4, 7]);
    expect(temperatureHistoryTickIndices(2)).toEqual([0, 1]);
  });
});

describe("savitzkyGolayTemperaturePlotSamples", () => {
  test("inserts drawing-only samples while preserving every real observation", () => {
    const temperatures = [20, 21, 24, 29, 36];

    const samples = savitzkyGolayTemperaturePlotSamples(temperatures, 2);

    expect(samples).toHaveLength(13);
    expect(samples.filter(({ virtual }) => !virtual)).toEqual(
      temperatures.map((temperature, position) => ({ position, temperature, virtual: false })),
    );
    expect(samples.filter(({ virtual }) => virtual)).toHaveLength(8);
  });

  test("uses the S-G trend for curved interpolation instead of linear midpoints", () => {
    const samples = savitzkyGolayTemperaturePlotSamples([0, 1, 4, 9, 16], 1);
    const midpoint = samples.find(({ position }) => position === 1.5);

    expect(midpoint?.virtual).toBe(true);
    expect(midpoint?.temperature).toBeCloseTo(2.275, 6);
  });

  test("keeps virtual drawing values inside the real observed range", () => {
    const temperatures = [10, 30, 11, 29, 12];
    const samples = savitzkyGolayTemperaturePlotSamples(temperatures);

    expect(samples.every(({ temperature }) => temperature >= 10 && temperature <= 30)).toBe(true);
  });
});
