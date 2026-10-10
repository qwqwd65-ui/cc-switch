import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  ProviderForm,
  type ProviderFormProps,
} from "@/components/providers/forms/ProviderForm";

vi.mock("@/components/providers/forms/McodeProviderForm", () => ({
  McodeProviderForm: (props: ProviderFormProps) => (
    <div>
      {props.upstreamProxyField}
      <span data-testid="request-proxy">
        {props.upstreamProxyUrl ?? "global"}
      </span>
      <button
        onClick={() =>
          void props.onSubmit({
            name: "test",
            settingsConfig: "{}",
            meta: { ...props.initialData?.meta, customUserAgent: "kept" },
          })
        }
      >
        Save provider
      </button>
    </div>
  ),
}));

describe("proxy metadata across the 4.0 specialized forms", () => {
  const initialData = {
    meta: {
      upstreamProxy: { enabled: true, url: "http://127.0.0.1:7890" },
      customUserAgent: "original",
    },
  };
  it("preserves the dedicated proxy alongside metadata edited by the child form", async () => {
    const submit = vi.fn();
    render(
      <ProviderForm
        appId="mcode"
        submitLabel="Save"
        onCancel={() => {}}
        onSubmit={submit}
        initialData={initialData}
      />,
    );
    expect(screen.getByTestId("request-proxy")).toHaveTextContent(
      "http://127.0.0.1:7890",
    );
    fireEvent.click(screen.getByText("Save provider"));
    await waitFor(() =>
      expect(submit).toHaveBeenCalledWith(
        expect.objectContaining({
          meta: expect.objectContaining({
            customUserAgent: "kept",
            upstreamProxy: initialData.meta.upstreamProxy,
          }),
        }),
      ),
    );
  });
  it("stops saving when an enabled proxy uses an unsupported protocol", async () => {
    const submit = vi.fn();
    render(
      <ProviderForm
        appId="mcode"
        submitLabel="Save"
        onCancel={() => {}}
        onSubmit={submit}
        initialData={initialData}
      />,
    );
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "file:///tmp/proxy" },
    });
    fireEvent.click(screen.getByText("Save provider"));
    expect(submit).not.toHaveBeenCalled();
  });
  it("clears request routing when disabled while keeping the address for later", async () => {
    const submit = vi.fn();
    render(
      <ProviderForm
        appId="mcode"
        submitLabel="Save"
        onCancel={() => {}}
        onSubmit={submit}
        initialData={initialData}
      />,
    );
    fireEvent.click(screen.getByRole("switch"));
    expect(screen.getByTestId("request-proxy")).toHaveTextContent("global");
    fireEvent.click(screen.getByText("Save provider"));
    await waitFor(() =>
      expect(submit).toHaveBeenCalledWith(
        expect.objectContaining({
          meta: expect.objectContaining({
            upstreamProxy: { enabled: false, url: "http://127.0.0.1:7890" },
          }),
        }),
      ),
    );
  });
});
