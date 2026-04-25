import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Cpu, Zap } from "lucide-react";
import { commands } from "@/bindings";

export const HardwareInfo: React.FC = () => {
  const { t } = useTranslation();
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

  if (!hardwareInfo) {
    return null;
  }

  return (
    <div className="flex items-center gap-3 px-3 py-2 bg-mid-gray/10 rounded-lg border border-mid-gray/20">
      <div className="flex items-center gap-2">
        {hardwareInfo.has_gpu ? (
          <>
            <Zap className="w-4 h-4 text-green-500" />
            <span className="text-sm font-medium">
              {t("settings.advanced.hardware.gpu")}
            </span>
          </>
        ) : (
          <>
            <Cpu className="w-4 h-4 text-blue-500" />
            <span className="text-sm font-medium">
              {t("settings.advanced.hardware.cpu")}
            </span>
          </>
        )}
      </div>
      <div className="text-sm text-mid-gray">
        {t("settings.advanced.hardware.cores", { count: hardwareInfo.cpu_cores })}
      </div>
    </div>
  );
};
