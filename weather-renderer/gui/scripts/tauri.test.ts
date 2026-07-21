import { describe, expect, test } from "bun:test";

import { prepareTauriArgs } from "./tauri.mjs";

describe("Tauri command defaults", () => {
  test("launches the GUI for an ordinary dev command", () => {
    expect(prepareTauriArgs(["dev"])).toEqual(["dev", "--", "--", "gui"]);
  });

  test("preserves runner arguments before adding the GUI selector", () => {
    expect(prepareTauriArgs(["dev", "--", "--release"])).toEqual([
      "dev",
      "--",
      "--release",
      "--",
      "gui",
    ]);
  });

  test("respects an explicit application mode", () => {
    expect(prepareTauriArgs(["dev", "--", "--", "tui"])).toEqual([
      "dev",
      "--",
      "--",
      "tui",
    ]);
  });

  test("does not alter build or help commands", () => {
    expect(prepareTauriArgs(["build"])).toEqual(["build"]);
    expect(prepareTauriArgs(["dev", "--help"])).toEqual(["dev", "--help"]);
  });
});
