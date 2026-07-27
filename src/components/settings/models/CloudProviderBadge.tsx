import React from "react";
import { Cloud } from "lucide-react";
import { useTranslation } from "react-i18next";

interface CloudProviderBadgeProps {
  beta?: boolean;
}

export const CloudProviderBadge: React.FC<CloudProviderBadgeProps> = ({
  beta = false,
}) => {
  const { t } = useTranslation();

  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-logo-primary/15 px-2 py-0.5 text-xs font-medium text-logo-primary">
      <Cloud className="h-3 w-3" aria-hidden="true" />
      {t("settings.models.cloud.badge")}
      {beta ? ` · ${t("settings.models.cloud.beta")}` : null}
    </span>
  );
};
