import { logError, run } from "@tauri-apps/cli";

const MODE_SELECTORS = new Set(["daemon", "tui", "gui"]);

export function prepareTauriArgs(input) {
  const args = [...input];
  if (args[0] !== "dev" || args.includes("--help") || args.includes("-h")) {
    return args;
  }

  const separators = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--") {
      separators.push(index);
    }
  }

  if (separators.length < 2) {
    args.push(...Array(2 - separators.length).fill("--"), "gui");
    return args;
  }

  const appArgsIndex = separators[1] + 1;
  if (!MODE_SELECTORS.has(args[appArgsIndex])) {
    args.splice(appArgsIndex, 0, "gui");
  }
  return args;
}

if (import.meta.main) {
  try {
    await run(prepareTauriArgs(process.argv.slice(2)), "bun run tauri");
  } catch (error) {
    logError(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
