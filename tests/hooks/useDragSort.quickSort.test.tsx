import type { ReactNode } from "react";
import { act, renderHook } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useDragSort } from "@/hooks/useDragSort";
import type { Provider } from "@/types";

const mocks = vi.hoisted(() => ({
  update: vi.fn(),
  tray: vi.fn(),
  error: vi.fn(),
}));
vi.mock("@/lib/api", () => ({
  providersApi: { updateSortOrder: mocks.update, updateTrayMenu: mocks.tray },
}));
vi.mock("@/lib/toast", () => ({
  toast: { success: vi.fn(), error: mocks.error },
}));

const providers = Object.fromEntries(
  ["a", "b", "c"].map((id, sortIndex) => [
    id,
    { id, name: id, sortIndex, settingsConfig: {} },
  ]),
) as Record<string, Provider>;
function setup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const invalidation = vi.spyOn(client, "invalidateQueries");
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return {
    ...renderHook(() => useDragSort(providers, "codex"), { wrapper }),
    invalidation,
  };
}
beforeEach(() => {
  mocks.update.mockReset().mockResolvedValue(true);
  mocks.tray.mockReset().mockResolvedValue(true);
  mocks.error.mockClear();
});
describe("quick sorting with the 4.0 routing queue", () => {
  it("moves the whole list, invalidates the routing queue, and refreshes the tray", async () => {
    const { result, invalidation } = setup();
    await act(async () => {
      await result.current.moveToBoundary("c", "top");
    });
    expect(mocks.update).toHaveBeenCalledWith(
      [
        { id: "c", sortIndex: 0 },
        { id: "a", sortIndex: 1 },
        { id: "b", sortIndex: 2 },
      ],
      "codex",
    );
    expect(invalidation).toHaveBeenCalledWith({
      queryKey: ["providers", "codex"],
    });
    expect(invalidation).toHaveBeenCalledWith({
      queryKey: ["failoverQueue", "codex"],
    });
    expect(mocks.tray).toHaveBeenCalledTimes(1);
  });
  it("ignores unknown or already positioned providers", async () => {
    const { result } = setup();
    await act(async () => {
      await result.current.moveToBoundary("a", "top");
      await result.current.moveToBoundary("c", "bottom");
      await result.current.moveToBoundary("deleted", "top");
    });
    expect(mocks.update).not.toHaveBeenCalled();
  });
  it("unblocks sorting after a failed database write without refreshing the tray", async () => {
    mocks.update.mockRejectedValueOnce(new Error("write failed"));
    const { result } = setup();
    await act(async () => {
      await result.current.moveToBoundary("a", "bottom");
    });
    expect(result.current.isSorting).toBe(false);
    expect(mocks.error).toHaveBeenCalled();
    expect(mocks.tray).not.toHaveBeenCalled();
    await act(async () => {
      await result.current.moveToBoundary("c", "top");
    });
    expect(mocks.tray).toHaveBeenCalledTimes(1);
  });
});
