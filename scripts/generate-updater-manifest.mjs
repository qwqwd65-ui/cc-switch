#!/usr/bin/env node
// Generates the Tauri v2 updater manifest for the fork release workflow.
// Every published platform entry must have both an artifact and a minisign
// signature; partial manifests fail instead of silently stranding users.
//
// Usage: node scripts/generate-updater-manifest.mjs <assets-dir> <tag> <repo> [output] [pub-date]

import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [assetsDir, tag, repo, output = "latest.json", pubDateArg] =
  process.argv.slice(2);

if (!assetsDir || !tag || !repo) {
  console.error(
    "Usage: node scripts/generate-updater-manifest.mjs <assets-dir> <tag> <repo> [output] [pub-date]",
  );
  process.exit(1);
}

const pubDate = pubDateArg ? new Date(pubDateArg) : new Date();
if (Number.isNaN(pubDate.getTime())) {
  console.error(`Invalid pub-date: ${pubDateArg}`);
  process.exit(1);
}

const names = readdirSync(assetsDir).sort();
const findOne = (suffix, label) => {
  const matches = names.filter((name) => name.endsWith(suffix));
  if (matches.length !== 1) {
    console.error(
      `Expected exactly one ${label} matching *${suffix}; found ${matches.length}`,
    );
    process.exit(1);
  }
  return matches[0];
};

const readSignedArtifact = (suffix, label) => {
  const name = findOne(suffix, label);
  const artifactPath = join(assetsDir, name);
  const signaturePath = `${artifactPath}.sig`;
  if (!existsSync(signaturePath)) {
    console.error(`Missing updater signature: ${name}.sig`);
    process.exit(1);
  }
  const signature = readFileSync(signaturePath, "utf8").trim();
  if (!signature) {
    console.error(`Updater signature is empty: ${name}.sig`);
    process.exit(1);
  }
  return { name, signature };
};

const windows = readSignedArtifact(
  "-Windows-Setup.exe",
  "Windows NSIS installer",
);
const macos = readSignedArtifact("-macOS.tar.gz", "macOS updater archive");
const linux = readSignedArtifact("-Linux-x86_64.AppImage", "Linux AppImage");
const baseUrl = `https://github.com/${repo}/releases/download/${tag}`;
const entry = ({ name, signature }) => ({
  signature,
  url: `${baseUrl}/${encodeURIComponent(name)}`,
});

const manifest = {
  version: tag.replace(/^v/, ""),
  notes: `CC Switch ${tag} fork release`,
  pub_date: pubDate.toISOString(),
  platforms: {
    "windows-x86_64": entry(windows),
    "darwin-aarch64": entry(macos),
    "darwin-x86_64": entry(macos),
    "linux-x86_64": entry(linux),
  },
};

writeFileSync(output, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(
  `Wrote ${output} for ${manifest.version} with ${Object.keys(manifest.platforms).length} platforms`,
);
