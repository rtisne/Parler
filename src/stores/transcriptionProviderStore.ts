import { create } from "zustand";
import {
  commands,
  type CredentialStatus,
  type ProviderDescriptor,
  type TranscriptionTargetId,
} from "@/bindings";

/**
 * State for cloud transcription providers: the catalog of descriptors, the
 * currently selected target, per-provider credential status, and accepted
 * consent versions.
 *
 * The secret value is never stored here — only whether a credential is
 * configured (`CredentialStatus.configured`). Credentials are write-only.
 */
interface TranscriptionProviderStore {
  providers: ProviderDescriptor[];
  selectedTarget: TranscriptionTargetId | null;
  credentialStatus: Record<string, CredentialStatus>;
  consents: Record<string, number>;
  loading: boolean;
  error: string | null;

  initialize: () => Promise<void>;
  refresh: () => Promise<void>;
  saveCredential: (providerId: string, secret: string) => Promise<boolean>;
  deleteCredential: (providerId: string) => Promise<string | null>;
  acceptConsent: (
    providerId: string,
    version: number,
  ) => Promise<string | null>;
  revokeConsent: (providerId: string) => Promise<string | null>;
  selectTarget: (
    target: TranscriptionTargetId | null,
  ) => Promise<string | null>;
}

export const useTranscriptionProviderStore = create<TranscriptionProviderStore>(
  (set, get) => ({
    providers: [],
    selectedTarget: null,
    credentialStatus: {},
    consents: {},
    loading: false,
    error: null,

    initialize: async () => {
      set({ loading: true });
      await get().refresh();
      set({ loading: false });
    },

    refresh: async () => {
      const providersResult = await commands.getCloudTranscriptionProviders();
      if (providersResult.status !== "ok") {
        set({ error: providersResult.error });
        return;
      }
      // Defensive: the backend already excludes the local provider, but this
      // store is the single source of truth for every cloud-only surface
      // (the settings list and the setup dialog), so never trust an
      // unfiltered catalog into activating a `local/<model>` target through
      // the cloud consent/credential path.
      const providers = providersResult.data.filter((p) => p.kind === "cloud");

      const targetResult = await commands.getSelectedTranscriptionTarget();
      const selectedTarget =
        targetResult.status === "ok" ? targetResult.data : null;

      const consentsResult = await commands.getCloudProviderConsents();
      const consents: Record<string, number> = {};
      if (consentsResult.status === "ok") {
        for (const [key, value] of Object.entries(consentsResult.data)) {
          if (typeof value === "number") consents[key] = value;
        }
      }

      const credentialStatus: Record<string, CredentialStatus> = {};
      for (const provider of providers) {
        const statusResult = await commands.getProviderCredentialStatus(
          provider.id,
        );
        if (statusResult.status === "ok") {
          credentialStatus[provider.id] = statusResult.data;
        }
      }

      set({
        providers,
        selectedTarget,
        consents,
        credentialStatus,
        error: null,
      });
    },

    saveCredential: async (providerId, secret) => {
      const result = await commands.setProviderCredential(providerId, secret);
      if (result.status !== "ok") {
        set({ error: result.error });
        return false;
      }
      await get().refresh();
      return true;
    },

    deleteCredential: async (providerId) => {
      const deletion = await commands.deleteProviderCredential(providerId);
      if (deletion.status !== "ok") {
        set({ error: deletion.error });
        return deletion.error;
      }
      await get().refresh();
      return null;
    },

    acceptConsent: async (providerId, version) => {
      const result = await commands.setCloudProviderConsent(
        providerId,
        version,
      );
      if (result.status !== "ok") {
        set({ error: result.error });
        return result.error;
      }
      await get().refresh();
      return null;
    },

    revokeConsent: async (providerId) => {
      const result = await commands.revokeCloudProviderConsent(providerId);
      if (result.status !== "ok") {
        set({ error: result.error });
        return result.error;
      }
      await get().refresh();
      return null;
    },

    selectTarget: async (target) => {
      const result = await commands.setSelectedTranscriptionTarget(target);
      if (result.status !== "ok") {
        set({ error: result.error });
        return result.error;
      }
      await get().refresh();
      return null;
    },
  }),
);
