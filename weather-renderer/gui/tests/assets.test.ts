import { describe, expect, test } from "bun:test";

import { weatherAtmosphere } from "../src/assets";

describe("weatherAtmosphere", () => {
  test("classifies Chinese and English weather descriptions", () => {
    expect(weatherAtmosphere("晴")).toBe("clear");
    expect(weatherAtmosphere("Partly cloudy")).toBe("cloudy");
    expect(weatherAtmosphere("小雨")).toBe("rain");
    expect(weatherAtmosphere("Thunderstorm")).toBe("storm");
    expect(weatherAtmosphere("雨夹雪")).toBe("snow");
    expect(weatherAtmosphere("轻雾")).toBe("fog");
  });

  test("uses the most safety-relevant condition for mixed descriptions", () => {
    expect(weatherAtmosphere("雷阵雨")).toBe("storm");
    expect(weatherAtmosphere("雨夹雪")).toBe("snow");
  });

  test("falls back to unknown when no condition can be inferred", () => {
    expect(weatherAtmosphere(null)).toBe("unknown");
    expect(weatherAtmosphere("—")).toBe("unknown");
  });
});
