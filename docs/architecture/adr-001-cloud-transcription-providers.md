# ADR-001: Cloud transcription provider contracts

- **Status:** Accepted (milestone 1 — ElevenLabs batch only)
- **Date:** 2026-07-22
- **Scope:** Freezes the official, verified API contracts required to add proprietary
  cloud transcription providers to Parler. Milestone 1 implements **ElevenLabs
  Scribe v2 batch** only. Azure Speech, AssemblyAI and Aqua/Avalon are documented
  here as **deferred**: their contracts must be frozen in a follow-up ADR revision
  before any client code is written.

> **Rule (from the roadmap):** every unknown field is written down as a _blocking
> question_, never assumed. Do not implement behaviour that depends on an open
> question without first resolving it against the live official documentation.

---

## 1. Context

`ModelManager` currently mixes local model files, the Gemini API and the CLI
client. We are introducing a `TranscriptionProvider` abstraction (see
`src-tauri/src/transcription/`) so that local and cloud targets share one
normalized contract. Before writing any network client we freeze the provider's
public HTTP contract here, sourced from official documentation, so the
implementation can be reviewed against a written reference rather than guesses.

Web search was available while writing this ADR (unlike when the roadmap plan was
drafted). All ElevenLabs facts below carry a source URL and were verified against
`elevenlabs.io/docs` as of **July 2026**. Where the official pages disagree or are
silent, the item is recorded as a **blocking question** and the implementation
uses the conservative choice noted.

---

## 2. Decision — ElevenLabs Scribe v2 (batch / pre-recorded)

Milestone 1 uses the **batch** (pre-recorded file) HTTP endpoint only. Realtime /
WebSocket streaming is explicitly out of scope for this PR and is a separate
future ADR revision + implementation.

### 2.1 Endpoint and authentication — CONFIRMED

| Item         | Value                                              | Source                                                                                         |
| ------------ | -------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Endpoint     | `POST https://api.elevenlabs.io/v1/speech-to-text` | [API ref — Create transcript](https://elevenlabs.io/docs/api-reference/speech-to-text/convert) |
| Auth header  | `xi-api-key: <API_KEY>`                            | [Authentication](https://elevenlabs.io/docs/api-reference/authentication)                      |
| Content type | `multipart/form-data`                              | [API ref](https://elevenlabs.io/docs/api-reference/speech-to-text/convert)                     |

The API key is stored **only** in the OS keyring (service `parler.transcription`,
account `provider/elevenlabs/api-key`) via the `SecretStore` from PR #19. It is
never written to settings JSON, logs, events, history, bindings or fixtures.

### 2.2 Request fields — CONFIRMED (subset used by Parler)

Multipart form fields. Parler sends only the minimal set required for a
French-first batch dictation; all other documented fields are intentionally left
at their server defaults.

| Field           | Type                                                            | Parler usage                                                           |
| --------------- | --------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `model_id`      | string                                                          | **`scribe_v2`** (current standard model; see Q1)                       |
| `file`          | binary                                                          | 16 kHz mono WAV encoded from the recorded `Vec<f32>` buffer            |
| `language_code` | string (ISO-639-1 / ISO-639-3), optional                        | Sent when the user picked a concrete language; omitted for auto-detect |
| `keyterms`      | string[] (≤1000 terms, <50 chars each, ≤5 words each), optional | Populated from the user's custom words when non-empty (see Q4)         |

Exactly one of `file` or `source_url` must be provided; Parler always uploads
`file`. Minimum audio length is 100 ms. Accepted containers include WAV, MP3,
FLAC, M4A, OGG, OPUS, WebM (audio) — Parler encodes WAV (PCM16, 16 kHz, mono),
mirroring the existing Gemini client's `encode_samples_to_wav`.
Source: [API ref](https://elevenlabs.io/docs/api-reference/speech-to-text/convert),
[Capabilities](https://elevenlabs.io/docs/overview/capabilities/speech-to-text).

### 2.3 Response schema — CONFIRMED

Single-channel success body (fields Parler reads in **bold**):

```jsonc
{
  "language_code": "fr", // detected/confirmed language  (read)
  "language_probability": 0.98, // confidence of detection
  "text": "Bonjour le monde.", // full transcript             (read, required)
  "words": [
    // word-level detail            (ignored in batch M1)
    {
      "text": "Bonjour",
      "start": 0.0,
      "end": 0.4,
      "type": "word",
      "speaker_id": null,
      "logprob": -0.1,
      "characters": null,
      "channel_index": null,
    },
  ],
}
```

Parler's batch path reads **`text`** (required) and **`language_code`** (optional,
stored as `detected_language`). `words[]`, diarization and timestamps are parsed
but unused in milestone 1. Source:
[API ref](https://elevenlabs.io/docs/api-reference/speech-to-text/convert).

### 2.4 Error contract — CONFIRMED

Error body nests everything under `detail`:

```jsonc
{
  "detail": {
    "type": "...",
    "code": "...",
    "message": "...",
    "status": "...",
    "request_id": "...",
    "param": "...",
  },
}
```

Status → normalized `TranscriptionError` category mapping used by the client:

| HTTP status | Meaning                                 | Normalized category    |
| ----------- | --------------------------------------- | ---------------------- |
| 400 / 422   | invalid/malformed params                | `InvalidConfiguration` |
| 401         | invalid / missing API key               | `Authentication`       |
| 402         | insufficient credits                    | `Quota`                |
| 403         | permission denied / IP not allow-listed | `Authentication`       |
| 429         | rate limit or concurrency limit         | `RateLimited`          |
| 5xx / 503   | server error / temporarily unavailable  | `ProviderUnavailable`  |
| (transport) | connect/read failure                    | `Network`              |
| (transport) | timeout                                 | `Timeout`              |

The client logs the category, the HTTP status and the sanitized `request_id`
only. It **never** logs the raw response body (it may contain transcript text or
sensitive details), the audio, or the API key. Sources:
[Errors](https://elevenlabs.io/docs/eleven-api/resources/errors),
[429](https://help.elevenlabs.io/hc/en-us/articles/19571824571921-API-Error-Code-429),
[400/401](https://help.elevenlabs.io/hc/en-us/articles/19572237925521-API-Error-Code-400-or-401-API-Key).

### 2.5 Cost, privacy and retention — CONFIRMED (with Q6)

| Item           | Value                                                                    | Source                                                                     |
| -------------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| Batch price    | **$0.22 / hour** of audio (≈ $0.0037/min)                                | [API pricing](https://elevenlabs.io/pricing/api)                           |
| Pricing page   | https://elevenlabs.io/pricing/api                                        | —                                                                          |
| Privacy policy | https://elevenlabs.io/privacy-policy                                     | —                                                                          |
| Zero-retention | Zero Retention Mode is **Enterprise-only**; covers `/v1/speech-to-text/` | [ZRM](https://elevenlabs.io/docs/eleven-api/resources/zero-retention-mode) |

These strings back the mandatory consent dialog: provider name, "your recorded
audio leaves this device", per-hour cost, pricing link, privacy link and the
"no automatic fallback" notice.

---

## 3. Blocking questions (resolve against live docs before relying on them)

| #   | Question                                                                                                                                                              | Conservative choice used in milestone 1                                                                                                   |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Q1  | Is `scribe_v1` still a supported production `model_id`, or is `scribe_v2` mandatory? Docs list both in the API reference but the capabilities page documents only v2. | Use `scribe_v2`. Keep `model_id` configurable in the target catalog so a future value needs no code change.                               |
| Q2  | Max file size: the capabilities page says 3 GB / 10 h, the API reference says <5.0 GB.                                                                                | Not enforced client-side in M1 (dictations are short); treat 3 GB / 10 h as the conservative documented ceiling if a guard is ever added. |
| Q3  | Are there sample-rate constraints for batch? Not documented.                                                                                                          | Send 16 kHz mono WAV (already produced by the pipeline); revisit if the API rejects it.                                                   |
| Q4  | Exact billing impact of `keyterms` (pricing page "+$0.05/h" vs API-ref "20% surcharge").                                                                              | Only send `keyterms` when the user has custom words; document the surcharge in the cost copy.                                             |
| Q5  | Is there an explicit "recommended model for French"? Not stated; French is in the top accuracy tier under Scribe v2.                                                  | Use `scribe_v2` for French.                                                                                                               |
| Q6  | STT-specific default retention duration is not stated in the privacy policy (only a 3-year voice-data figure).                                                        | Consent copy links the privacy policy and states audio leaves the device; it does not claim a specific retention period.                  |

None of these block the milestone-1 batch path, which depends only on CONFIRMED
items in §2. They are recorded so later work (realtime, keyterm tuning, large
files) resolves them explicitly rather than inheriting an assumption.

---

## 4. Deferred providers (contracts NOT frozen in this PR)

These are **out of scope** for milestone 1. No client code is written for them
here. Each must have its contract frozen (endpoint, auth, recommended French
model, PCM/framing, partial/final events, close, cancel, timeout, size limits,
quota, price, retention) in a revision of this ADR **before** implementation, and
each must plug into the same `TranscriptionProvider` registry and generic setup
dialog — no provider-specific branching in the central pipeline.

- **Azure Speech-to-Text** (roadmap Lot 5): decide REST/WebSocket vs SDK; region
  vs endpoint; subscription key vs Entra ID token. Blocking until frozen.
- **AssemblyAI Streaming** (roadmap Lot 6): Universal model params for French,
  endpointing, auth, close semantics. Blocking until frozen.
- **Aqua / Avalon** (roadmap Lot 7): **on hold** — no public API, terms,
  redistribution rights or documentation confirmed. Do not implement a
  hypothetical client. Kept out of the visible target list until validated.

---

## 5. Consequences

- Milestone 1 can implement a fully testable ElevenLabs batch client whose
  request-building, response-parsing and status→error mapping are pure functions
  covered by unit tests with local fixtures; no test contacts the real provider.
- The registry/descriptor model means Azure, AssemblyAI and Aqua become additive:
  a new `TranscriptionProvider` implementation plus a catalog entry, with no
  changes to the recording/correction/post-processing/history/paste pipeline.
- Open questions are contained to non-blocking tuning concerns; the shipped path
  depends only on confirmed contract facts.
