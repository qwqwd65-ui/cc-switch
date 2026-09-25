import { getVersion } from "@tauri-apps/api/app";

const RELEASES_API_URL =
  "https://api.github.com/repos/qwqwd65-ui/cc-switch/releases?per_page=10";
const RELEASES_PAGE_URL = "https://github.com/qwqwd65-ui/cc-switch/releases";

export type UpdateChannel = "stable" | "beta";

export interface UpdateInfo {
  currentVersion: string;
  availableVersion: string;
  notes?: string;
  pubDate?: string;
  releaseUrl: string;
}

export interface UpdateProgressEvent {
  event: "Started" | "Progress" | "Finished";
  total?: number;
  downloaded?: number;
}

export interface UpdateHandle {
  version: string;
  notes?: string;
  date?: string;
  releaseUrl: string;
}

export interface CheckOptions {
  timeout?: number;
  channel?: UpdateChannel;
}

interface GitHubRelease {
  tag_name?: string;
  body?: string | null;
  published_at?: string | null;
  html_url?: string;
  draft?: boolean;
  prerelease?: boolean;
}

function normalizeVersion(version: string): string {
  return version.trim().replace(/^v/i, "");
}

function parseCore(version: string): number[] {
  return normalizeVersion(version)
    .split("-", 1)[0]
    .split(".")
    .map((part) => Number.parseInt(part, 10) || 0);
}

function parseSuffix(version: string): Array<number | string> {
  const normalized = normalizeVersion(version);
  const dashIndex = normalized.indexOf("-");
  if (dashIndex === -1) return [];

  const suffix = normalized.slice(dashIndex + 1);
  return (suffix.match(/[A-Za-z]+|\d+/g) ?? []).map((part) => {
    const numeric = Number.parseInt(part, 10);
    return Number.isNaN(numeric) ? part.toLowerCase() : numeric;
  });
}

function compareToken(a: number | string, b: number | string): number {
  if (typeof a === "number" && typeof b === "number") {
    return a - b;
  }
  if (typeof a === "number") return -1;
  if (typeof b === "number") return 1;
  return a.localeCompare(b);
}

function compareVersion(a: string, b: string): number {
  const coreA = parseCore(a);
  const coreB = parseCore(b);
  const coreLength = Math.max(coreA.length, coreB.length);

  for (let i = 0; i < coreLength; i += 1) {
    const diff = (coreA[i] ?? 0) - (coreB[i] ?? 0);
    if (diff !== 0) return diff;
  }

  const suffixA = parseSuffix(a);
  const suffixB = parseSuffix(b);

  if (suffixA.length === 0 && suffixB.length === 0) return 0;
  if (suffixA.length === 0) return 1;
  if (suffixB.length === 0) return -1;

  const suffixLength = Math.max(suffixA.length, suffixB.length);
  for (let i = 0; i < suffixLength; i += 1) {
    if (i >= suffixA.length) return -1;
    if (i >= suffixB.length) return 1;
    const diff = compareToken(suffixA[i], suffixB[i]);
    if (diff !== 0) return diff;
  }

  return 0;
}

async function fetchLatestRelease(
  timeout: number,
  channel: UpdateChannel,
): Promise<GitHubRelease | null> {
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), timeout);

  try {
    const response = await fetch(RELEASES_API_URL, {
      method: "GET",
      headers: {
        Accept: "application/vnd.github+json",
      },
      signal: controller.signal,
    });

    if (!response.ok) {
      throw new Error(`GitHub releases request failed: ${response.status}`);
    }

    const releases = (await response.json()) as GitHubRelease[];
    return (
      releases.find(
        (release) =>
          !release.draft &&
          release.tag_name &&
          (channel === "beta" || !release.prerelease),
      ) ?? null
    );
  } finally {
    window.clearTimeout(timer);
  }
}

export async function getCurrentVersion(): Promise<string> {
  try {
    return await getVersion();
  } catch {
    return "";
  }
}

export async function checkForUpdate(
  opts: CheckOptions = {},
): Promise<
  { status: "up-to-date" } | { status: "available"; info: UpdateInfo }
> {
  const currentVersion = await getCurrentVersion();
  const release = await fetchLatestRelease(
    opts.timeout ?? 30000,
    opts.channel ?? "stable",
  );

  if (!release?.tag_name) {
    return { status: "up-to-date" };
  }

  const availableVersion = normalizeVersion(release.tag_name);
  if (
    !currentVersion ||
    compareVersion(availableVersion, currentVersion) <= 0
  ) {
    return { status: "up-to-date" };
  }

  const releaseUrl =
    release.html_url || `${RELEASES_PAGE_URL}/tag/${release.tag_name}`;
  const info: UpdateInfo = {
    currentVersion,
    availableVersion,
    notes: release.body ?? undefined,
    pubDate: release.published_at ?? undefined,
    releaseUrl,
  };

  return { status: "available", info };
}

export async function relaunchApp(): Promise<void> {
  return Promise.resolve();
}
