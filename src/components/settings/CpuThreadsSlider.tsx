import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Slider } from "../ui/Slider";
import { useSettings } from "../../hooks/useSettings";
import { commands } from "@/bindings";

interface CpuThreadsSliderProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const CpuThreadsSlider: React.FC<CpuThreadsSliderProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();
  const cpuThreads = getSetting("cpu_threads") ?? 4;
  const [hardwareInfo, setHardwareInfo] = useState<{
    has_gpu: boolean;
    cpu_cores: number;
    recommended_threads: number;
  } | null>(null);

  useEffect(() => {
    commands.getHardwareInfo().then((info) => {
      setHardwareInfo(info);
    });
  }, []);

  const maxThreads = hardwareInfo?.cpu_cores ?? 16;
  const recommendedThreads = hardwareInfo?.recommended_threads ?? 4;

  return (
    <Slider
      value={cpuThreads}
      onChange={(value: number) => updateSetting("cpu_threads", Math.round(value))}
      min={1}
      max={maxThreads}
      step={1}
      label={t("settings.advanced.cpuThreads.title")}
      description={t("settings.advanced.cpuThreads.description", {
        recommended: recommendedThreads,
      })}
      descriptionMode={descriptionMode}
      grouped={grouped}
      formatValue={(value) => `${Math.round(value)} ${t("settings.advanced.cpuThreads.threads")}`}
    />
  );
};
