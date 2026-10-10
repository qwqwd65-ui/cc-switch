import { useTranslation } from "react-i18next";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  validateProviderUpstreamProxy,
  type ProviderUpstreamProxyFormConfig,
} from "@/lib/providerUpstreamProxy";

interface ProviderUpstreamProxyFieldProps {
  id: string;
  config: ProviderUpstreamProxyFormConfig;
  onChange: (config: ProviderUpstreamProxyFormConfig) => void;
}

export function ProviderUpstreamProxyField({
  id,
  config,
  onChange,
}: ProviderUpstreamProxyFieldProps) {
  const { t } = useTranslation();
  const error = validateProviderUpstreamProxy(config);

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-1">
          <Label htmlFor={`${id}-enabled`}>
            {t("providerForm.upstreamProxy", {
              defaultValue: "供应商专属代理",
            })}
          </Label>
          <p className="text-xs text-muted-foreground">
            {t("providerForm.upstreamProxyHint", {
              defaultValue:
                "CC Switch 对该供应商发起的模型获取、额度查询和路由请求使用此代理；直连客户端由其自身网络设置控制。",
            })}
          </p>
        </div>
        <Switch
          id={`${id}-enabled`}
          checked={config.enabled}
          onCheckedChange={(enabled) => onChange({ ...config, enabled })}
        />
      </div>

      <Input
        id={`${id}-url`}
        value={config.url}
        onChange={(event) => onChange({ ...config, url: event.target.value })}
        placeholder="http://127.0.0.1:7890 / socks5://127.0.0.1:1080"
        className="font-mono text-sm"
        disabled={!config.enabled}
        aria-invalid={Boolean(error)}
      />
      {error && (
        <p className="text-xs text-destructive">
          {t("providerForm.upstreamProxyInvalid", {
            error,
            defaultValue: error,
          })}
        </p>
      )}
    </div>
  );
}
