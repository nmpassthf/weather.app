import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const guiDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workspace = resolve(guiDir, "..", "..");
const targetTriple = process.env.WEATHER_CARGO_TARGET?.trim();
const cargoArgs = ["build", "-p", "weather-daemon", "--release"];
if (targetTriple) cargoArgs.push("--target", targetTriple);

const result = spawnSync("cargo", cargoArgs, { cwd: workspace, stdio: "inherit" });
if (result.status !== 0) process.exit(result.status ?? 1);

const windowsTarget = targetTriple ? targetTriple.includes("windows") : process.platform === "win32";
const executable = windowsTarget ? "weather-daemon.exe" : "weather-daemon";
const source = resolve(workspace, "target", ...(targetTriple ? [targetTriple] : []), "release", executable);
const destination = resolve(guiDir, "src-tauri", "resources", "bin", executable);
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);
console.log(`staged ${source} -> ${destination}`);
