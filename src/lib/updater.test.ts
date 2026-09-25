import { beforeEach, describe, expect, it, vi } from "vitest";
import { getVersion } from "@tauri-apps/api/app";
import { checkForUpdate } from "./updater";

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
  });

  it("checks the fork repository and ignores prereleases on the stable channel", async () => {
    fetchMock.mockResolvedValue(
      githubResponse([
        {
          tag_name: "v3.21.0-fork.1-beta.1",
          prerelease: true,
          draft: false,
          html_url:
            "https://github.com/qwqwd65-ui/cc-switch/releases/tag/v3.21.0-fork.1-beta.1",
        },
        {
          tag_name: "v3.20.5-fork.1",
          prerelease: false,
          draft: false,
          html_url:
            "https://github.com/qwqwd65-ui/cc-switch/releases/tag/v3.20.5-fork.1",
        },
      ]),
    );

    await expect(checkForUpdate()).resolves.toMatchObject({
      status: "available",
      info: {
        currentVersion: "3.20.4-fork.1",
        availableVersion: "3.20.5-fork.1",
      },
    });
    expect(fetchMock).toHaveBeenCalledWith(
      "https://api.github.com/repos/qwqwd65-ui/cc-switch/releases?per_page=10",
      expect.objectContaining({ method: "GET" }),
    );
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
