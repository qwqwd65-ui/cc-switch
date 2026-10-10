import type {
  Provider,
  OpenCodeProviderConfig,
  OpenClawProviderConfig,
} from "@/types";
import type { AppId } from "@/lib/api";
import { extractCodexModelName } from "@/utils/providerConfigUtils";

type ModelBadgeInfo = {
  label: string;
  title: string;
};

const CLAUDE_ONE_M_MARKER = "[1m]";

const stripClaudeOneMMarker = (model: string): string => {
  const trimmedEnd = model.trimEnd();
  if (!trimmedEnd.toLowerCase().endsWith(CLAUDE_ONE_M_MARKER)) {
    return model.trim();
  }
  return trimmedEnd.slice(0, -CLAUDE_ONE_M_MARKER.length).trimEnd();
};

const envString = (env: Record<string, any> | undefined, key: string) => {
  const value = env?.[key];
  return typeof value === "string" && value.trim() ? value.trim() : null;
};

const extractClaudeModelBadge = (
  env: Record<string, any> | undefined,
): ModelBadgeInfo | null => {
  const roleModels = [
    {
      role: "Opus",
      model: envString(env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
    },
    {
      role: "Sonnet",
      model: envString(env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
    },
    {
      role: "Haiku",
      model: envString(env, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
    },
  ]
    .map(({ role, model }) => ({
      role,
      model: model ? stripClaudeOneMMarker(model) : null,
    }))
    .filter(
      (item): item is { role: string; model: string } => item.model !== null,
    );

  if (roleModels.length === 0) {
    const fallbackModel = envString(env, "ANTHROPIC_MODEL");
    if (!fallbackModel) return null;
    const label = stripClaudeOneMMarker(fallbackModel);
    return { label, title: label };
  }

  const title = roleModels
    .map(({ role, model }) => `${role}: ${model}`)
    .join(" / ");
  const hasAllRoleModels = roleModels.length === 3;
  const uniqueModels = new Set(roleModels.map(({ model }) => model));

  if (hasAllRoleModels && uniqueModels.size === 1) {
    const label = roleModels[0].model;
    return { label, title };
  }

  const primary = roleModels[0];
  return {
    label: primary.model,
    title,
  };
};

export const extractModelBadge = (
  provider: Provider,
  appId: AppId,
): ModelBadgeInfo | null => {
  const config = provider.settingsConfig;
  if (!config || typeof config !== "object") return null;

  const env = (config as Record<string, any>)?.env;

  if (appId === "claude") {
    return extractClaudeModelBadge(env);
  }

  if (appId === "gemini") {
    const model = env?.GEMINI_MODEL;
    if (typeof model === "string" && model.trim()) {
      const label = model.trim();
      return { label, title: label };
    }
  }

  if (appId === "codex") {
    const toml = (config as Record<string, any>)?.config;
    if (typeof toml === "string") {
      const model = extractCodexModelName(toml);
      if (model) return { label: model, title: model };
    }
  }

  if (appId === "opencode") {
    const openCodeConfig = config as OpenCodeProviderConfig;
    const models = openCodeConfig?.models;
    if (models && typeof models === "object") {
      const modelKeys = Object.keys(models);
      if (modelKeys.length > 0) {
        const firstModelKey = modelKeys[0];
        const firstModel = models[firstModelKey];
        const label = firstModel.name || firstModelKey;
        return { label, title: label };
      }
    }
  }

  if (appId === "openclaw") {
    const openClawConfig = config as OpenClawProviderConfig;
    const models = openClawConfig?.models;
    if (Array.isArray(models) && models.length > 0) {
      const firstModel = models[0];
      const label = firstModel.name || firstModel.id;
      return { label, title: label };
    }
  }

  if (appId === "hermes") {
    const models = (config as Record<string, any>)?.models;
    if (Array.isArray(models) && models.length > 0) {
      const firstModel = models[0];
      const label = firstModel.name || firstModel.id;
      return { label, title: label };
    }
  }

  if (appId === "grokbuild") {
    const models = (config as Record<string, any>)?.models;
    if (Array.isArray(models) && models.length > 0) {
      const firstModel = models[0];
      const label = firstModel.name || firstModel.id;
      return { label, title: label };
    }
  }

  return null;
};
