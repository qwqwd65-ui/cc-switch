import { beforeEach, describe, expect, it, vi } from "vitest";
import { getVersion } from "@tauri-apps/api/app";
import { checkForUpdate } from "./updater";
const updaterMocks = vi.hoisted(() => ({ check: vi.fn(), close: vi.fn() }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: updaterMocks.check }));

vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn(),
}));

const mockedGetVersion = vi.mocked(getVersion);

function githubResponse(releases: unknown[]) {
  return {
    ok: true,
    json: vi.fn().mockResolvedValue(releases),
  } as unknown as Response;
}

describe("fork updater release discovery", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", fetchMock);
    mockedGetVersion.mockResolvedValue("3.20.4-fork.1");
    updaterMocks.check.mockReset().mockResolvedValue(null);
    updaterMocks.close.mockReset().mockResolvedValue(undefined);
  });

  it("discovers the stable version through the same updater as the installer", async () => {
    updaterMocks.check.mockResolvedValue({
      version: "4.0.7-fork.1",
      body: "Fork compatibility changes",
      date: "2026-10-10T09:11:51Z",
      close: updaterMocks.close,
    });

    await expect(checkForUpdate()).resolves.toMatchObject({
      status: "available",
      info: {
        currentVersion: "3.20.4-fork.1",
        availableVersion: "4.0.7-fork.1",
        notes: "Fork compatibility changes",
        releaseUrl:
          "https://github.com/qwqwd65-ui/cc-switch/releases/tag/v4.0.7-fork.1",
      },
    });
    expect(updaterMocks.check).toHaveBeenCalledWith({ timeout: 30000 });
    expect(updaterMocks.close).toHaveBeenCalledTimes(1);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("does not advertise a release absent from the installer manifest", async () => {
    await expect(checkForUpdate()).resolves.toEqual({ status: "up-to-date" });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("surfaces a manifest failure without switching update sources", async () => {
    updaterMocks.check.mockRejectedValue(new Error("manifest unavailable"));
    await expect(checkForUpdate()).rejects.toThrow("manifest unavailable");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("allows prereleases when the beta channel is requested", async () => {
    fetchMock.mockResolvedValue(
      githubResponse([
        {
          tag_name: "v3.21.0-fork.1-beta.1",
          prerelease: true,
          draft: false,
          html_url:
            "https://github.com/qwqwd65-ui/cc-switch/releases/tag/v3.21.0-fork.1-beta.1",
        },
      ]),
    );

    await expect(checkForUpdate({ channel: "beta" })).resolves.toMatchObject({
      status: "available",
      info: { availableVersion: "3.21.0-fork.1-beta.1" },
    });
  });
});
