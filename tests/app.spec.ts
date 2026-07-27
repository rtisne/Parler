import { test, expect, type Page } from "@playwright/test";

const installTauriMock = async (page: Page) => {
  await page.addInitScript(() => {
    const callbacks = new Map<number, (...args: unknown[]) => void>();
    let callbackId = 1;
    const cloud = {
      configured: false,
      consentVersion: 0,
      selected: null as null | { provider_id: string; model_id: string },
      failRevoke: false,
      failModelSelection: false,
      calls: [] as Array<{ command: string; args: Record<string, unknown> }>,
    };
    Object.assign(window, { __cloudMock: cloud });

    const localModel = {
      id: "small",
      name: "Small",
      description: "Local model",
      size: 1,
      url: "",
      filename: "small.bin",
      is_downloaded: true,
      is_downloading: false,
      is_custom: false,
      engine_type: "whisper",
      supported_languages: ["en", "fr"],
      recommended: true,
    };
    const secondLocalModel = {
      ...localModel,
      id: "medium",
      name: "Medium",
      description: "Second local model",
      filename: "medium.bin",
      recommended: false,
    };

    const settings = {
      bindings: {},
      push_to_talk: true,
      audio_feedback: false,
      selected_model: "small",
      selected_language: "auto",
      custom_words: [],
      post_process_providers: [],
      post_process_api_keys: {},
      post_process_models: {},
      post_process_prompts: [],
      post_process_actions: [],
      saved_processing_models: [],
      cloud_provider_consents: {},
      selected_transcription_target: {
        provider_id: "local",
        model_id: "small",
      },
      external_script_path: null,
    };

    const invoke = async (
      command: string,
      args: Record<string, unknown> = {},
    ) => {
      cloud.calls.push({ command, args });
      if (command.includes("identifier")) return "com.parler.dev";
      if (command.includes("platform")) return "windows";
      if (command.includes("listen")) return 1;
      if (command.includes("unlisten")) return null;
      switch (command) {
        case "has_any_models_available":
          return true;
        case "get_app_settings":
        case "get_default_settings":
          return settings;
        case "get_available_models":
          return [localModel, secondLocalModel];
        case "get_current_model":
          return "small";
        case "get_transcription_model_status":
          return "small";
        case "set_active_model":
          if (cloud.failModelSelection) throw new Error("load_failed");
          return null;
        case "get_available_microphones":
        case "get_available_output_devices":
          return [];
        case "get_cloud_transcription_providers":
          // Mirrors the real registry, which also holds the local provider
          // (see providers.rs's `cloud_only` filter and issue #32): a mock
          // that returned only cloud descriptors would never exercise the
          // frontend's own defensive `kind === "cloud"` filter in
          // transcriptionProviderStore.ts.
          return [
            {
              id: "local",
              label: "Local",
              kind: "local",
              models: [{ id: "small", label: "Small" }],
              capabilities: {
                batch: true,
                realtime: false,
                supported_languages: [],
                supports_word_timestamps: false,
                sends_audio_off_device: false,
              },
              requires_credential: false,
              privacy_url: null,
              pricing_url: null,
              cost_text: null,
              retention_text: null,
              consent_version: 0,
              beta: false,
            },
            {
              id: "elevenlabs",
              label: "ElevenLabs Scribe",
              kind: "cloud",
              models: [{ id: "scribe_v2", label: "Scribe v2 (batch)" }],
              capabilities: {
                batch: true,
                realtime: false,
                supported_languages: [],
                supports_word_timestamps: true,
                sends_audio_off_device: true,
              },
              requires_credential: true,
              privacy_url: "https://example.invalid/privacy",
              pricing_url: "https://example.invalid/pricing",
              cost_text: "about $0.22/hour",
              retention_text: "Retention follows provider account settings.",
              consent_version: 1,
              beta: true,
            },
          ];
        case "get_selected_transcription_target":
          return cloud.selected ?? settings.selected_transcription_target;
        case "get_cloud_provider_consents":
          return cloud.consentVersion
            ? { elevenlabs: cloud.consentVersion }
            : {};
        case "get_provider_credential_status":
          return { configured: cloud.configured, backend_available: true };
        case "set_provider_credential":
          cloud.configured = true;
          return null;
        case "delete_provider_credential":
          cloud.configured = false;
          if (cloud.selected?.provider_id === String(args.providerId)) {
            cloud.selected = null;
          }
          return null;
        case "set_cloud_provider_consent":
          cloud.consentVersion = Number(args.version);
          return null;
        case "test_cloud_provider_connection":
          return null;
        case "revoke_cloud_provider_consent":
          if (cloud.failRevoke) throw "revoke_failed";
          cloud.consentVersion = 0;
          cloud.selected = null;
          return null;
        case "set_selected_transcription_target":
          cloud.selected = (args.target as typeof cloud.selected) ?? null;
          return null;
        default:
          return null;
      }
    };

    Object.assign(window, {
      __TAURI_OS_PLUGIN_INTERNALS__: {
        platform: "windows",
        family: "windows",
        version: "11",
        os_type: "windows_nt",
        arch: "x86_64",
        exe_extension: "exe",
        eol: "\r\n",
      },
      __TAURI_EVENT_PLUGIN_INTERNALS__: {
        unregisterListener: () => undefined,
      },
      __TAURI_INTERNALS__: {
        invoke,
        transformCallback: (
          callback: (...args: unknown[]) => void,
          once = false,
        ) => {
          const id = callbackId++;
          callbacks.set(id, (...args: unknown[]) => {
            callback(...args);
            if (once) callbacks.delete(id);
          });
          return id;
        },
        unregisterCallback: (id: number) => callbacks.delete(id),
        metadata: {
          currentWindow: { label: "main" },
          currentWebview: { label: "main" },
        },
      },
    });
  });
};

const openCloudSettings = async (page: Page) => {
  await installTauriMock(page);
  await page.goto("/");
  await page.getByRole("button", { name: /models/i }).click();
  await expect(page.getByTestId("cloud-provider-elevenlabs")).toBeVisible();
  await page
    .getByTestId("cloud-provider-elevenlabs")
    .getByRole("button")
    .click();
  await expect(page.getByTestId("provider-setup-dialog")).toBeVisible();
};

test.describe("Parler App", () => {
  test("the local provider from a mixed catalog is never rendered as a cloud provider", async ({
    page,
  }) => {
    await installTauriMock(page);
    await page.goto("/");
    await page.getByRole("button", { name: "Models" }).click();

    await expect(page.getByTestId("cloud-provider-elevenlabs")).toBeVisible();
    await expect(page.getByTestId("cloud-provider-local")).toHaveCount(0);
  });

  test("a failed local model load does not persist a different target", async ({
    page,
  }) => {
    await installTauriMock(page);
    await page.goto("/");
    await page.getByRole("button", { name: "Models" }).click();
    await page.evaluate(() => {
      (
        window as typeof window & {
          __cloudMock: { failModelSelection: boolean };
        }
      ).__cloudMock.failModelSelection = true;
    });

    await page.getByRole("button").filter({ hasText: "Medium" }).click();

    const state = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: {
              failModelSelection: boolean;
              calls: Array<{ command: string; args: Record<string, unknown> }>;
            };
          }
        ).__cloudMock,
    );
    const targetWrites = state.calls.filter(
      ({ command }) => command === "set_selected_transcription_target",
    );
    expect(state.failModelSelection).toBe(true);
    expect(targetWrites, JSON.stringify(state.calls)).toHaveLength(0);
  });

  test("dev server responds", async ({ page }) => {
    const response = await page.goto("/");
    expect(response?.status()).toBe(200);
  });

  test("cloud activation requires explicit consent and keeps the secret write-only", async ({
    page,
  }) => {
    await openCloudSettings(page);
    const secret = page.getByTestId("provider-api-key");
    await expect(secret).toHaveValue("");
    await secret.fill("test-only-key");
    await expect(page.getByTestId("provider-activate")).toBeDisabled();
    await page.getByTestId("provider-consent").check();
    await expect(page.getByTestId("provider-activate")).toBeEnabled();
    await page.getByTestId("provider-activate").click();
    await expect(page.getByTestId("provider-setup-dialog")).toBeHidden();

    const state = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: {
              selected: unknown;
              configured: boolean;
              consentVersion: number;
            };
          }
        ).__cloudMock,
    );
    expect(state.configured).toBe(true);
    expect(state.consentVersion).toBe(1);
    expect(state.selected).toEqual({
      provider_id: "elevenlabs",
      model_id: "scribe_v2",
    });

    await page
      .getByTestId("cloud-provider-elevenlabs")
      .getByRole("button")
      .click();
    await expect(page.getByTestId("provider-api-key")).toHaveValue("");
    await expect(
      page.getByText("Retention follows provider account settings."),
    ).toBeVisible();
    await page.getByRole("button", { name: /test connection/i }).click();
    const connectionTests = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: { calls: Array<{ command: string }> };
          }
        ).__cloudMock.calls.filter(
          (call) => call.command === "test_cloud_provider_connection",
        ).length,
    );
    expect(connectionTests).toBe(1);
  });

  test("cancel does not change the selected transcription target", async ({
    page,
  }) => {
    await openCloudSettings(page);
    await page.getByTestId("provider-api-key").fill("not-saved");
    await page.getByRole("button", { name: /cancel/i }).click();
    const selected = await page.evaluate(
      () =>
        (window as typeof window & { __cloudMock: { selected: unknown } })
          .__cloudMock.selected,
    );
    expect(selected).toBeNull();
  });

  test("revoking consent disables the cloud target", async ({ page }) => {
    await openCloudSettings(page);
    await page.getByTestId("provider-api-key").fill("test-only-key");
    await page.getByTestId("provider-consent").check();
    await page.getByTestId("provider-activate").click();
    await page
      .getByTestId("cloud-provider-elevenlabs")
      .getByRole("button")
      .click();
    await page.getByRole("button", { name: /revoke consent/i }).click();
    const state = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: { selected: unknown; consentVersion: number };
          }
        ).__cloudMock,
    );
    expect(state.selected).toBeNull();
    expect(state.consentVersion).toBe(0);
  });

  test("deleting the active credential disables the cloud target", async ({
    page,
  }) => {
    await openCloudSettings(page);
    await page.getByTestId("provider-api-key").fill("test-only-key");
    await page.getByTestId("provider-consent").check();
    await page.getByTestId("provider-activate").click();
    await page
      .getByTestId("cloud-provider-elevenlabs")
      .getByRole("button")
      .click();
    await page.getByRole("button", { name: /delete credential/i }).click();

    const state = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: { selected: unknown; configured: boolean };
          }
        ).__cloudMock,
    );
    expect(state.configured).toBe(false);
    expect(state.selected).toBeNull();
  });

  test("a failed revocation remains visible and does not close the dialog", async ({
    page,
  }) => {
    await openCloudSettings(page);
    await page.getByTestId("provider-api-key").fill("test-only-key");
    await page.getByTestId("provider-consent").check();
    await page.getByTestId("provider-activate").click();
    await page
      .getByTestId("cloud-provider-elevenlabs")
      .getByRole("button")
      .click();
    await page.evaluate(() => {
      (
        window as typeof window & { __cloudMock: { failRevoke: boolean } }
      ).__cloudMock.failRevoke = true;
    });

    await page.getByRole("button", { name: /revoke consent/i }).click();
    await expect(page.getByTestId("provider-setup-dialog")).toBeVisible();
    await expect(page.getByTestId("provider-operation-error")).toBeVisible();
    const state = await page.evaluate(
      () =>
        (
          window as typeof window & {
            __cloudMock: { selected: unknown; consentVersion: number };
          }
        ).__cloudMock,
    );
    expect(state.selected).toEqual({
      provider_id: "elevenlabs",
      model_id: "scribe_v2",
    });
    expect(state.consentVersion).toBe(1);
  });
});
