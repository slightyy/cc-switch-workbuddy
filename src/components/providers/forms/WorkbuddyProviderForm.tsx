import { useEffect, useMemo, useState } from "react";
import { useForm } from "react-hook-form";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { Form } from "@/components/ui/form";
import { ImeSafeInput } from "@/components/ui/ime-safe-input";
import { Label } from "@/components/ui/label";
import { useProvidersQuery } from "@/lib/query/queries";
import type { ProviderFormData } from "@/lib/schemas/provider";
import { BasicFormFields } from "./BasicFormFields";
import type { ProviderFormProps } from "./ProviderForm";

/** `~/.workbuddy/models.json` 的一个元素。 */
interface WorkbuddyEntry {
  id: string;
  name: string;
  vendor: string;
  url: string;
  apiKey: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
  supportsReasoning: boolean;
  useCustomProtocol: boolean;
}

const ID_PATTERN = /^[A-Za-z0-9._\-/]+$/;

const emptyEntry = (): WorkbuddyEntry => ({
  id: "",
  name: "",
  vendor: "Custom",
  url: "",
  apiKey: "",
  supportsToolCall: true,
  supportsImages: false,
  supportsReasoning: false,
  useCustomProtocol: false,
});

const toEntry = (config?: Record<string, unknown>): WorkbuddyEntry => {
  const base = emptyEntry();
  if (!config) return base;
  return {
    id: typeof config.id === "string" ? config.id : base.id,
    name: typeof config.name === "string" ? config.name : base.name,
    vendor: typeof config.vendor === "string" ? config.vendor : base.vendor,
    url: typeof config.url === "string" ? config.url : base.url,
    apiKey: typeof config.apiKey === "string" ? config.apiKey : base.apiKey,
    supportsToolCall: config.supportsToolCall !== false,
    supportsImages: config.supportsImages === true,
    supportsReasoning: config.supportsReasoning === true,
    useCustomProtocol: config.useCustomProtocol === true,
  };
};

const isHttpUrl = (value: string) => {
  try {
    const url = new URL(value.trim());
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
};

export function WorkbuddyProviderForm({
  appId,
  providerId,
  initialData,
  onSubmit,
  onCancel,
  submitLabel,
  showButtons = true,
  onSubmittingChange,
  onSubmitReadyChange,
}: ProviderFormProps) {
  const { t } = useTranslation();
  const isEdit = Boolean(initialData);
  const [entry, setEntry] = useState<WorkbuddyEntry>(() =>
    toEntry(initialData?.settingsConfig),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const { data: existing } = useProvidersQuery(appId);
  const form = useForm<ProviderFormData>({
    defaultValues: {
      name: initialData?.name ?? "",
      notes: initialData?.notes ?? "",
      websiteUrl: initialData?.websiteUrl ?? "",
      icon: initialData?.icon ?? "",
      iconColor: initialData?.iconColor ?? "",
      settingsConfig: JSON.stringify(initialData?.settingsConfig ?? {}, null, 2),
    },
  });
  const name = form.watch("name");
  // 编辑时主键固定：它同时是 models.json 里那条记录的 `id`。
  const entryId = isEdit ? (providerId ?? entry.id) : entry.id;
  const keyTaken = useMemo(
    () =>
      !isEdit &&
      Object.prototype.hasOwnProperty.call(
        existing?.providers ?? {},
        entry.id.trim(),
      ),
    [existing, entry.id, isEdit],
  );
  const idInvalid = entry.id !== "" && !ID_PATTERN.test(entry.id.trim());
  const ready = Boolean(
    name.trim() &&
      entryId.trim() &&
      !idInvalid &&
      !keyTaken &&
      isHttpUrl(entry.url) &&
      entry.apiKey.trim(),
  );

  useEffect(() => {
    onSubmitReadyChange?.(ready);
  }, [ready, onSubmitReadyChange]);

  const update = (patch: Partial<WorkbuddyEntry>) =>
    setEntry((current) => ({ ...current, ...patch }));

  function Checkbox({
    label,
    checked,
    onChange,
  }: {
    label: string;
    checked: boolean;
    onChange: (value: boolean) => void;
  }) {
    return (
      <label className="flex cursor-pointer items-center gap-2 text-sm text-fg-2">
        <input
          type="checkbox"
          className="h-4 w-4 accent-emerald-600"
          checked={checked}
          onChange={(event) => onChange(event.target.checked)}
        />
        {label}
      </label>
    );
  }

  return (
    <Form {...form}>
      <form
        id="provider-form"
        className="space-y-6"
        onSubmit={form.handleSubmit(async (identity) => {
          if (!ready || busy) return;
          setBusy(true);
          onSubmittingChange?.(true);
          setError("");
          try {
            const payload: WorkbuddyEntry = {
              ...entry,
              id: entryId.trim(),
              name: identity.name.trim(),
              url: entry.url.trim(),
              apiKey: entry.apiKey.trim(),
            };
            await onSubmit({
              ...identity,
              name: identity.name.trim(),
              meta: initialData?.meta,
              providerKey: entryId.trim(),
              presetCategory: initialData?.category ?? "custom",
              settingsConfig: JSON.stringify(payload),
            });
          } catch (submitError) {
            setError(String(submitError));
          } finally {
            setBusy(false);
            onSubmittingChange?.(false);
          }
        })}
      >
        {error && (
          <p role="alert" className="text-sm text-destructive">
            {error}
          </p>
        )}
        <fieldset
          disabled={busy}
          className="min-w-0 space-y-6 border-0 p-0 disabled:opacity-50"
        >
          <BasicFormFields
            form={form}
            beforeNameSlot={
              <div className="space-y-2">
                <Label htmlFor="workbuddy-model-id">
                  {t("workbuddy.modelId", "模型 ID")}
                  <span aria-hidden="true" className="text-destructive ml-1">
                    *
                  </span>
                </Label>
                <ImeSafeInput
                  id="workbuddy-model-id"
                  value={entry.id}
                  onValueChange={(value) => update({ id: value })}
                  disabled={isEdit}
                  placeholder={t(
                    "workbuddy.modelIdPlaceholder",
                    "deepseek-chat / glm-5.3-flash",
                  )}
                  autoComplete="off"
                  className={keyTaken || idInvalid ? "border-destructive" : ""}
                />
                <p
                  className={
                    keyTaken || idInvalid
                      ? "text-xs text-destructive"
                      : "text-xs text-fg-2"
                  }
                >
                  {keyTaken
                    ? t(
                        "workbuddy.modelIdDuplicate",
                        "这个模型 ID 已在 WorkBuddy 里存在",
                      )
                    : idInvalid
                      ? t(
                          "workbuddy.modelIdInvalid",
                          "只能包含字母、数字和 . _ - /",
                        )
                      : isEdit
                        ? t(
                            "workbuddy.modelIdLocked",
                            "模型 ID 已写入 WorkBuddy，编辑时不可修改",
                          )
                        : t(
                            "workbuddy.modelIdHint",
                            "必须与上游供应商提供的模型名完全一致",
                          )}
                </p>
              </div>
            }
          />

          <div className="space-y-2">
            <Label htmlFor="workbuddy-url">
              {t("workbuddy.url", "接口地址")}
              <span aria-hidden="true" className="text-destructive ml-1">
                *
              </span>
            </Label>
            <ImeSafeInput
              id="workbuddy-url"
              value={entry.url}
              onValueChange={(value) => update({ url: value })}
              placeholder="https://api.deepseek.com/v1/chat/completions"
              autoComplete="off"
            />
            <p className="text-xs text-fg-2">
              {t(
                "workbuddy.urlHint",
                "WorkBuddy 只支持 OpenAI 兼容接口，通常以 /chat/completions 结尾",
              )}
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="workbuddy-api-key">
              {t("workbuddy.apiKey", "API Key")}
              <span aria-hidden="true" className="text-destructive ml-1">
                *
              </span>
            </Label>
            <ImeSafeInput
              id="workbuddy-api-key"
              value={entry.apiKey}
              onValueChange={(value) => update({ apiKey: value })}
              placeholder="sk-..."
              autoComplete="off"
            />
          </div>

          <div className="space-y-3">
            <Label>{t("workbuddy.capabilities", "模型能力")}</Label>
            <div className="grid gap-2 sm:grid-cols-2">
              <Checkbox
                label={t("workbuddy.supportsToolCall", "支持工具调用")}
                checked={entry.supportsToolCall}
                onChange={(value) => update({ supportsToolCall: value })}
              />
              <Checkbox
                label={t("workbuddy.supportsImages", "支持图片输入")}
                checked={entry.supportsImages}
                onChange={(value) => update({ supportsImages: value })}
              />
              <Checkbox
                label={t("workbuddy.supportsReasoning", "支持深度思考")}
                checked={entry.supportsReasoning}
                onChange={(value) => update({ supportsReasoning: value })}
              />
              <Checkbox
                label={t("workbuddy.customProtocol", "自定义协议路径")}
                checked={entry.useCustomProtocol}
                onChange={(value) => update({ useCustomProtocol: value })}
              />
            </div>
            <p className="text-xs text-fg-2">
              {t(
                "workbuddy.capabilitiesHint",
                "关闭工具调用后 WorkBuddy 的 Agent 与 Skill 无法使用；地址不以 chat/completions 结尾时请打开自定义协议路径",
              )}
            </p>
          </div>
        </fieldset>
        {showButtons && (
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="outline"
              onClick={onCancel}
              disabled={busy}
            >
              {t("common.cancel")}
            </Button>
            <Button type="submit" disabled={busy || !ready}>
              {submitLabel}
            </Button>
          </div>
        )}
      </form>
    </Form>
  );
}
