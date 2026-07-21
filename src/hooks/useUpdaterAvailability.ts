import { useEffect, useState } from "react";
import { commands } from "../bindings";

interface UpdaterAvailability {
  isConfigured: boolean;
  isLoading: boolean;
}

export const useUpdaterAvailability = (): UpdaterAvailability => {
  const [isConfigured, setIsConfigured] = useState(false);
  const [isLoading, setIsLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;

    commands
      .isUpdaterConfigured()
      .then((configured) => {
        if (!cancelled) {
          setIsConfigured(configured);
        }
      })
      .catch((error) => {
        console.error("Failed to read updater configuration:", error);
      })
      .finally(() => {
        if (!cancelled) {
          setIsLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, []);

  return { isConfigured, isLoading };
};
