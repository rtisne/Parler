import React, { useEffect, useMemo, useState } from "react";
import { ExternalLink, ShieldAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import type {
  CredentialStatus,
  ProviderDescriptor,
  TranscriptionTargetId,
} from "@/bindings";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { CloudProviderBadge } from "./CloudProviderBadge";

interface ProviderSetupDialogProps {
  open: boolean;
  provider: ProviderDescriptor | null;
  credentialStatus?: CredentialStatus;
  acceptedConsentVersion: number;
  onClose: () => void;
  onSaveCredential: (providerId: string, secret: string) => Promise<boolean>;
  onDeleteCredential: (providerId: string) => Promise<string | null>;
  onAcceptConsent: (
    providerId: string,
    version: number,
  ) => Promise<string | null>;
  onRevokeConsent: (providerId: string) => Promise<string | null>;
  onSelectTarget: (target: TranscriptionTargetId) => Promise<string | null>;
  onTestConnection: (providerId: string) => Promise<string | null>;
}

export const ProviderSetupDialog: React.FC<ProviderSetupDialogProps> = ({
  open,
  provider,
  credentialStatus,
  acceptedConsentVersion,
  onClose,
  onSaveCredential,
  onDeleteCredential,
  onAcceptConsent,
  onRevokeConsent,
  onSelectTarget,
  onTestConnection,
}) => {
  const { t } = useTranslation();
  const [secret, setSecret] = useState("");
  const [consentChecked, setConsentChecked] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      // Credentials are deliberately write-only and are never read back.
      setSecret("");
      setConsentChecked(false);
      setError(null);
    }
  }, [open, provider?.id]);

  const hasCurrentConsent = useMemo(
    () => !!provider && acceptedConsentVersion >= provider.consent_version,
    [acceptedConsentVersion, provider],
  );

  if (!open || !provider) return null;

  const activate = async () => {
    setSaving(true);
    setError(null);
    try {
      if (provider.requires_credential && !credentialStatus?.configured) {
        if (!secret.trim()) {
          setError(t("settings.models.cloud.errors.missingCredential"));
          return;
        }
        const saved = await onSaveCredential(provider.id, secret.trim());
        if (!saved) {
          setError(t("settings.models.cloud.errors.credentialSave"));
          return;
        }
      } else if (secret.trim()) {
        const saved = await onSaveCredential(provider.id, secret.trim());
        if (!saved) {
          setError(t("settings.models.cloud.errors.credentialSave"));
          return;
        }
      }

      if (!hasCurrentConsent) {
        if (!consentChecked) {
          setError(t("settings.models.cloud.errors.consentRequired"));
          return;
        }
        const consentError = await onAcceptConsent(
          provider.id,
          provider.consent_version,
        );
        if (consentError) {
          setError(t("settings.models.cloud.errors.operationFailed"));
          return;
        }
      }

      const model = provider.models[0];
      if (!model) {
        setError(t("settings.models.cloud.errors.noModel"));
        return;
      }
      const selectionError = await onSelectTarget({
        provider_id: provider.id,
        model_id: model.id,
      });
      if (selectionError) {
        setError(
          t(`settings.models.cloud.errors.${selectionError}`, selectionError),
        );
        return;
      }
      setSecret("");
      onClose();
    } finally {
      setSaving(false);
    }
  };

  const revoke = async () => {
    setSaving(true);
    setError(null);
    try {
      const revokeError = await onRevokeConsent(provider.id);
      if (revokeError) {
        setError(t("settings.models.cloud.errors.operationFailed"));
        return;
      }
      onClose();
    } finally {
      setSaving(false);
    }
  };

  const removeCredential = async () => {
    setSaving(true);
    setError(null);
    try {
      const deletionError = await onDeleteCredential(provider.id);
      if (deletionError) {
        setError(t("settings.models.cloud.errors.operationFailed"));
        return;
      }
      setSecret("");
    } finally {
      setSaving(false);
    }
  };

  const testConnection = async () => {
    setSaving(true);
    setError(null);
    try {
      const testError = await onTestConnection(provider.id);
      if (testError) {
        setError(t("settings.models.cloud.errors.operationFailed"));
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="cloud-provider-title"
      data-testid="provider-setup-dialog"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="max-h-[90vh] w-full max-w-lg space-y-4 overflow-y-auto rounded-xl border border-mid-gray/40 bg-background p-5 shadow-2xl">
        <div className="flex items-start justify-between gap-3">
          <div>
            <div className="mb-2">
              <CloudProviderBadge beta={provider.beta} />
            </div>
            <h2 id="cloud-provider-title" className="text-lg font-semibold">
              {t("settings.models.cloud.configure", {
                provider: provider.label,
              })}
            </h2>
          </div>
          <Button variant="ghost" size="sm" onClick={onClose}>
            {t("settings.models.cloud.cancel")}
          </Button>
        </div>

        <div className="space-y-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3 text-sm">
          <div className="flex items-start gap-2 font-medium text-amber-600 dark:text-amber-300">
            <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0" />
            <span>{t("settings.models.cloud.offDeviceWarning")}</span>
          </div>
          <p>
            {t("settings.models.cloud.costWarning", {
              cost:
                provider.cost_text ?? t("settings.models.cloud.variableCost"),
            })}
          </p>
          {provider.id === "elevenlabs" ? (
            <p>{t("settings.models.cloud.keytermsCostWarning")}</p>
          ) : null}
          {provider.retention_text ? (
            <p>
              <strong>{t("settings.models.cloud.privacy")}:</strong>{" "}
              {provider.retention_text}
            </p>
          ) : null}
          <p>{t("settings.models.cloud.noFallback")}</p>
          <div className="flex flex-wrap gap-3 pt-1">
            {provider.privacy_url ? (
              <a
                className="inline-flex items-center gap-1 text-logo-primary underline"
                href={provider.privacy_url}
                target="_blank"
                rel="noreferrer"
              >
                {t("settings.models.cloud.privacy")}{" "}
                <ExternalLink className="h-3 w-3" />
              </a>
            ) : null}
            {provider.pricing_url ? (
              <a
                className="inline-flex items-center gap-1 text-logo-primary underline"
                href={provider.pricing_url}
                target="_blank"
                rel="noreferrer"
              >
                {t("settings.models.cloud.pricing")}{" "}
                <ExternalLink className="h-3 w-3" />
              </a>
            ) : null}
          </div>
        </div>

        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 rounded-lg border border-mid-gray/20 p-3 text-sm">
          <dt className="font-medium">
            {t("settings.models.cloud.processingMode")}
          </dt>
          <dd>
            {provider.capabilities.batch
              ? t("settings.models.cloud.batchMode")
              : t("settings.models.cloud.realtimeUnavailable")}
          </dd>
          <dt className="font-medium">
            {t("settings.models.cloud.languages")}
          </dt>
          <dd>
            {provider.capabilities.supported_languages.length > 0
              ? provider.capabilities.supported_languages.join(", ")
              : t("settings.models.cloud.autoLanguage")}
          </dd>
          {!provider.capabilities.realtime ? (
            <dd className="col-span-2 text-xs text-text/55">
              {t("settings.models.cloud.realtimeUnavailable")}
            </dd>
          ) : null}
        </dl>

        <div className="space-y-1">
          <label htmlFor="provider-api-key" className="text-sm font-medium">
            {t("settings.models.cloud.apiKey")}
          </label>
          <Input
            id="provider-api-key"
            data-testid="provider-api-key"
            type="password"
            autoComplete="new-password"
            value={secret}
            onChange={(event) => setSecret(event.target.value)}
            placeholder={
              credentialStatus?.configured
                ? t("settings.models.cloud.credentialConfigured")
                : t("settings.models.cloud.apiKeyPlaceholder")
            }
            disabled={!credentialStatus?.backend_available}
          />
          <p className="text-xs text-text/55">
            {t("settings.models.cloud.writeOnly")}
          </p>
          {!credentialStatus?.backend_available ? (
            <p className="text-xs text-red-500">
              {t("settings.models.cloud.keyringUnavailable")}
            </p>
          ) : null}
        </div>

        {!hasCurrentConsent ? (
          <label className="flex cursor-pointer items-start gap-2 text-sm">
            <input
              data-testid="provider-consent"
              type="checkbox"
              checked={consentChecked}
              onChange={(event) => setConsentChecked(event.target.checked)}
              className="mt-1"
            />
            <span>
              {t("settings.models.cloud.consent", { provider: provider.label })}
            </span>
          </label>
        ) : (
          <p className="text-sm text-green-600">
            {t("settings.models.cloud.consentAccepted")}
          </p>
        )}

        {error ? (
          <p
            data-testid="provider-operation-error"
            className="text-sm text-red-500"
          >
            {error}
          </p>
        ) : null}

        <div className="flex flex-wrap justify-end gap-2 pt-1">
          <Button
            variant="secondary"
            size="sm"
            onClick={testConnection}
            disabled={saving || !credentialStatus?.configured}
          >
            {t("settings.models.cloud.testConnection")}
          </Button>
          {credentialStatus?.configured ? (
            <Button
              variant="secondary"
              size="sm"
              onClick={removeCredential}
              disabled={saving}
            >
              {t("settings.models.cloud.deleteCredential")}
            </Button>
          ) : null}
          {hasCurrentConsent ? (
            <Button
              variant="secondary"
              size="sm"
              onClick={revoke}
              disabled={saving}
            >
              {t("settings.models.cloud.revokeConsent")}
            </Button>
          ) : null}
          <Button
            variant="primary"
            size="sm"
            data-testid="provider-activate"
            onClick={activate}
            disabled={
              saving ||
              !credentialStatus?.backend_available ||
              (!hasCurrentConsent && !consentChecked) ||
              (provider.requires_credential &&
                !credentialStatus?.configured &&
                !secret.trim())
            }
          >
            {saving
              ? t("settings.models.cloud.saving")
              : t("settings.models.cloud.activate")}
          </Button>
        </div>
      </div>
    </div>
  );
};
