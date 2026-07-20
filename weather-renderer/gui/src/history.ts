import type { PassedWeatherChart } from "./types";

const NMC_DATE_TIME = /^(\d{4})-(\d{1,2})-(\d{1,2})[T\s](\d{1,2}):(\d{2})(?::(\d{2}))?/u;
const DEFAULT_VIRTUAL_POINTS_PER_SEGMENT = 4;

export type TemperaturePlotSample = {
  position: number;
  temperature: number;
  virtual: boolean;
};

function observationTimestamp(value: string | null | undefined): number | null {
  const match = value?.trim().match(NMC_DATE_TIME);
  if (!match) return null;

  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const hour = Number(match[4]);
  const minute = Number(match[5]);
  const second = Number(match[6] ?? "0");
  const timestamp = Date.UTC(year, month - 1, day, hour, minute, second);
  const parsed = new Date(timestamp);
  if (parsed.getUTCFullYear() !== year
    || parsed.getUTCMonth() !== month - 1
    || parsed.getUTCDate() !== day
    || parsed.getUTCHours() !== hour
    || parsed.getUTCMinutes() !== minute
    || parsed.getUTCSeconds() !== second) return null;
  return timestamp;
}

export function recentTemperatureHistoryRows(
  passedchart: readonly PassedWeatherChart[],
  limit = 18,
): PassedWeatherChart[] {
  if (limit <= 0) return [];
  const rows = passedchart.filter(
    (row) => typeof row.temperature === "number" && Number.isFinite(row.temperature),
  );
  const timestamps = rows.map((row) => observationTimestamp(row.time));

  if (timestamps.every((timestamp): timestamp is number => timestamp !== null)) {
    return rows
      .map((row, index) => ({ row, timestamp: timestamps[index] as number, index }))
      .sort((left, right) => left.timestamp - right.timestamp || left.index - right.index)
      .slice(-limit)
      .map(({ row }) => row);
  }

  // NMC returns passedchart newest-first. Preserve that contract for legacy rows
  // without a complete date, then reverse only the selected window for plotting.
  return rows.slice(0, limit).reverse();
}

export function temperatureHistoryTickIndices(length: number): number[] {
  if (length <= 0) return [];
  if (length === 18) return [0, 3, 6, 9, 12, 15];
  const tickCount = Math.min(length, length >= 9 ? 5 : 3);
  if (tickCount === 1) return [0];
  return Array.from(
    { length: tickCount },
    (_, index) => Math.round((index * (length - 1)) / (tickCount - 1)),
  );
}

function finiteDifferenceSlopes(values: readonly number[]): number[] {
  return values.map((value, index) => {
    if (values.length === 1) return 0;
    if (index === 0) return (values[1] ?? value) - value;
    if (index === values.length - 1) return value - (values[index - 1] ?? value);
    return ((values[index + 1] ?? value) - (values[index - 1] ?? value)) / 2;
  });
}

function savitzkyGolaySlopes(values: readonly number[]): number[] {
  if (values.length < 5) return finiteDifferenceSlopes(values);
  const sample = (index: number): number => values[Math.max(0, Math.min(values.length - 1, index))] ?? 0;
  // Five-point, second-order Savitzky-Golay first-derivative kernel.
  return values.map((_, index) => (
    -2 * sample(index - 2)
    - sample(index - 1)
    + sample(index + 1)
    + 2 * sample(index + 2)
  ) / 10);
}

export function savitzkyGolayTemperaturePlotSamples(
  temperatures: readonly number[],
  virtualPointsPerSegment = DEFAULT_VIRTUAL_POINTS_PER_SEGMENT,
): TemperaturePlotSample[] {
  if (temperatures.length === 0) return [];
  const virtualPointCount = Math.max(0, Math.floor(virtualPointsPerSegment));
  const slopes = savitzkyGolaySlopes(temperatures);
  const min = Math.min(...temperatures);
  const max = Math.max(...temperatures);
  const samples: TemperaturePlotSample[] = [{
    position: 0,
    temperature: temperatures[0] ?? 0,
    virtual: false,
  }];

  for (let index = 0; index < temperatures.length - 1; index += 1) {
    const start = temperatures[index] ?? 0;
    const end = temperatures[index + 1] ?? start;
    const startSlope = slopes[index] ?? 0;
    const endSlope = slopes[index + 1] ?? 0;
    for (let offset = 1; offset <= virtualPointCount; offset += 1) {
      const t = offset / (virtualPointCount + 1);
      const t2 = t * t;
      const t3 = t2 * t;
      const interpolated = (2 * t3 - 3 * t2 + 1) * start
        + (t3 - 2 * t2 + t) * startSlope
        + (-2 * t3 + 3 * t2) * end
        + (t3 - t2) * endSlope;
      samples.push({
        position: index + t,
        temperature: Math.max(min, Math.min(max, interpolated)),
        virtual: true,
      });
    }
    samples.push({ position: index + 1, temperature: end, virtual: false });
  }

  return samples;
}
