import { describe, expect, it } from "vitest";
import { extractModelBadge } from "@/components/providers/modelBadge";
import type { Provider } from "@/types";

const provider = (settingsConfig: Provider["settingsConfig"]): Provider => ({
  id: "test",
  name: "test",
  settingsConfig,
});
describe("fork model badges on the 4.0 cards", () => {
  it("keeps all Claude role names in the tooltip and shortens the visible label", () => {
    expect(
      extractModelBadge(
        provider({
          env: {
            ANTHROPIC_DEFAULT_OPUS_MODEL: "opus[1m]",
            ANTHROPIC_DEFAULT_SONNET_MODEL: "sonnet",
            ANTHROPIC_DEFAULT_HAIKU_MODEL: "haiku",
          },
        }),
        "claude",
      ),
    ).toEqual({
      label: "opus",
      title: "Opus: opus / Sonnet: sonnet / Haiku: haiku",
    });
  });
  it("reads a single quoted top-level Codex model without confusing provider table fields", () => {
    expect(
      extractModelBadge(
        provider({
          config:
            "model = 'my-model'\n[model_providers.custom]\nmodel = 'wrong'\n",
        }),
        "codex",
      ),
    ).toEqual({ label: "my-model", title: "my-model" });
    expect(
      extractModelBadge(
        provider({ config: "[model_providers.custom]\nmodel = 'wrong'\n" }),
        "codex",
      ),
    ).toBeNull();
  });
  it("leaves an unconfigured card without a model badge", () => {
    expect(extractModelBadge(provider({ env: {} }), "claude")).toBeNull();
  });
});
