import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { useUpdaterAvailability } from "../../hooks/useUpdaterAvailability";

interface UpdateChecksToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const UpdateChecksToggle: React.FC<UpdateChecksToggleProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const { isConfigured, isLoading } = useUpdaterAvailability();
  const updateChecksEnabled =
    isConfigured && (getSetting("update_checks_enabled") ?? false);

  return (
    <ToggleSwitch
      checked={updateChecksEnabled}
      onChange={(enabled) => updateSetting("update_checks_enabled", enabled)}
      disabled={isLoading || !isConfigured}
      isUpdating={isUpdating("update_checks_enabled")}
      label={t("settings.debug.updateChecks.label")}
      description={t(
        isConfigured
          ? "settings.debug.updateChecks.description"
          : "footer.updateCheckingUnavailable",
      )}
      descriptionMode={descriptionMode}
      grouped={grouped}
    />
  );
};
