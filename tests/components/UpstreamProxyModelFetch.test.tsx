import { act, fireEvent, screen } from "@testing-library/react";
import { useForm } from "react-hook-form";
import type { PropsWithChildren } from "react";
import { describe, expect, it, vi } from "vitest";
import { Form } from "@/components/ui/form";
import { OpenClawFormFields } from "@/components/providers/forms/OpenClawFormFields";
import { HermesFormFields } from "@/components/providers/forms/HermesFormFields";
import { fetchModelsForConfig } from "@/lib/api/model-fetch";
import { renderWithQueryClient as render } from "../utils/testQueryClient";

vi.mock("@/lib/api/model-fetch", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api/model-fetch")>()),
  fetchModelsForConfig: vi.fn(),
}));

function FormShell({ children }: PropsWithChildren) {
  const form = useForm();
  return <Form {...form}>{children}</Form>;
}

function ModelForm({
  kind,
  upstreamProxyUrl,
}: {
  kind: "openclaw" | "hermes";
  upstreamProxyUrl: string;
}) {
  const common = {
    baseUrl: "https://api.example.com/v1",
    onBaseUrlChange: vi.fn(),
    apiKey: "sk-test",
    onApiKeyChange: vi.fn(),
    shouldShowApiKeyLink: false,
    websiteUrl: "",
    models: [{ id: "configured-model", name: "Configured Model" }],
    onModelsChange: vi.fn(),
    upstreamProxyUrl,
  };
  return (
    <FormShell>
      {kind === "openclaw" ? (
        <OpenClawFormFields
          {...common}
          api="openai-responses"
          onApiChange={vi.fn()}
          userAgent={false}
          onUserAgentChange={vi.fn()}
        />
      ) : (
        <HermesFormFields
          {...common}
          apiMode="chat_completions"
          onApiModeChange={vi.fn()}
          rateLimitDelay={undefined}
          onRateLimitDelayChange={vi.fn()}
        />
      )}
    </FormShell>
  );
}

describe.each(["openclaw", "hermes"] as const)(
  "%s model fetching with a provider proxy",
  (kind) => {
    it("uses the new proxy and ignores a response from the previous proxy", async () => {
      let resolveOld!: (
        models: Awaited<ReturnType<typeof fetchModelsForConfig>>,
      ) => void;
      let resolveNew!: (
        models: Awaited<ReturnType<typeof fetchModelsForConfig>>,
      ) => void;
      vi.mocked(fetchModelsForConfig)
        .mockImplementationOnce(
          () =>
            new Promise((resolve) => {
              resolveOld = resolve;
            }),
        )
        .mockImplementationOnce(
          () =>
            new Promise((resolve) => {
              resolveNew = resolve;
            }),
        );
      const { rerender } = render(
        <ModelForm kind={kind} upstreamProxyUrl="http://127.0.0.1:7890" />,
      );
      const fetchButton = screen.getByRole("button", {
        name: "providerForm.fetchModels",
      });
      fireEvent.click(fetchButton);
      expect(fetchModelsForConfig).toHaveBeenLastCalledWith(
        "https://api.example.com/v1",
        "sk-test",
        undefined,
        undefined,
        undefined,
        { upstreamProxyUrl: "http://127.0.0.1:7890" },
      );
      rerender(
        <ModelForm kind={kind} upstreamProxyUrl="http://127.0.0.1:7891" />,
      );
      fireEvent.click(fetchButton);
      expect(fetchModelsForConfig).toHaveBeenLastCalledWith(
        "https://api.example.com/v1",
        "sk-test",
        undefined,
        undefined,
        undefined,
        { upstreamProxyUrl: "http://127.0.0.1:7891" },
      );
      await act(async () => {
        resolveOld([{ id: "stale-model", ownedBy: null }]);
      });
      expect(fetchButton).toBeDisabled();
      expect(
        screen.queryByRole("button", { name: "Select model" }),
      ).not.toBeInTheDocument();
      await act(async () => {
        resolveNew([{ id: "fresh-model", ownedBy: null }]);
      });
      expect(fetchButton).toBeEnabled();
      expect(
        screen.getByRole("button", { name: "Select model" }),
      ).toBeInTheDocument();
      Element.prototype.scrollIntoView = vi.fn();
      fireEvent.click(screen.getByRole("button", { name: "Select model" }));
      expect(
        screen.getByRole("option", { name: "fresh-model" }),
      ).toBeInTheDocument();
      expect(
        screen.queryByRole("option", { name: "stale-model" }),
      ).not.toBeInTheDocument();
    });
  },
);
