import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface PreloadModelProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const PreloadModel: React.FC<PreloadModelProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const preloadEnabled = getSetting("preload_model_on_startup") ?? false;

  return (
    <ToggleSwitch
      checked={preloadEnabled}
      onChange={(enabled) => updateSetting("preload_model_on_startup", enabled)}
      isUpdating={isUpdating("preload_model_on_startup")}
      label={t("settings.advanced.preloadModel.label")}
      description={t("settings.advanced.preloadModel.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    />
  );
};
