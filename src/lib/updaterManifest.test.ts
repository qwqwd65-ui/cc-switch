import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";

const manifestScript = path.resolve(
  process.cwd(),
  "scripts/generate-updater-manifest.mjs",
);
const tempDirs: string[] = [];

function createAssets() {
  const dir = mkdtempSync(path.join(tmpdir(), "cc-switch-updater-"));
  tempDirs.push(dir);
  for (const name of [
    "CC-Switch-v3.20.4-fork.1-Windows-Setup.exe",
    "CC-Switch-v3.20.4-fork.1-macOS.tar.gz",
    "CC-Switch-v3.20.4-fork.1-Linux-x86_64.AppImage",
  ]) {
    writeFileSync(path.join(dir, name), "artifact");
    writeFileSync(path.join(dir, `${name}.sig`), `signature-${name}`);
  }
  return dir;
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("generate-updater-manifest", () => {
  it("requires signed assets and emits the four Tauri platform entries", () => {
    const assetsDir = createAssets();
    const output = path.join(assetsDir, "latest.json");

    execFileSync(process.execPath, [
      manifestScript,
      assetsDir,
      "v3.20.4-fork.1",
      "qwqwd65-ui/cc-switch",
      output,
      "2026-09-25T00:00:00Z",
    ]);

    const manifest = JSON.parse(readFileSync(output, "utf8"));
    expect(manifest.version).toBe("3.20.4-fork.1");
    expect(Object.keys(manifest.platforms).sort()).toEqual([
      "darwin-aarch64",
      "darwin-x86_64",
      "linux-x86_64",
      "windows-x86_64",
    ]);
    expect(manifest.platforms["windows-x86_64"].url).toBe(
      "https://github.com/qwqwd65-ui/cc-switch/releases/download/v3.20.4-fork.1/CC-Switch-v3.20.4-fork.1-Windows-Setup.exe",
    );
  });

  it("fails rather than publishing a platform without a signature", () => {
    const assetsDir = createAssets();
    rmSync(
      path.join(assetsDir, "CC-Switch-v3.20.4-fork.1-Windows-Setup.exe.sig"),
    );

    expect(() =>
      execFileSync(process.execPath, [
        manifestScript,
        assetsDir,
        "v3.20.4-fork.1",
        "qwqwd65-ui/cc-switch",
        path.join(assetsDir, "latest.json"),
      ]),
    ).toThrow();
  });
});
