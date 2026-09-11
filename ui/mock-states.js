// Fixture for the states gallery (ui/states.html) — every FetchStatus the UI can
// render, plus the two Claude multi-account sentinels (`token expired`,
// `no credentials`) and the `usageKnown: false` lane.
//
// Hand-written on purpose: `ui/mock.js` is generated from the CLI's JSON and must
// stay byte-identical to what the backend serves, so demo-only snapshots live
// here instead. Shape is still exactly `ProviderSnapshot` (CONTRACT.md).
window.__CODEXBAR_STATES__ = {
  schemaVersion: 1,
  generatedAt: "2026-09-10T21:24:10.850Z",
  providers: [
    {
      provider: "codex",
      title: "Codex",
      account: "user@example.com",
      plan: "Plus",
      windows: [
        {
          id: "session",
          title: "Session · 5h",
          kind: "session",
          window: { usedPercent: 18.4, windowMinutes: 300, resetsAt: "2026-09-10T23:11:00.000Z" }
        },
        {
          id: "weekly",
          title: "Weekly · 7d",
          kind: "weekly",
          window: { usedPercent: 9.2, windowMinutes: 10080, resetsAt: "2026-09-14T12:00:00.000Z" }
        }
      ],
      status: "ok",
      source: "oauth",
      fetchedAt: "2026-09-10T21:23:58.000Z"
    },
    {
      provider: "claude",
      title: "Claude",
      account: "user@example.com",
      plan: "Max 20x",
      windows: [
        {
          id: "session",
          title: "Session · 5h",
          kind: "session",
          window: { usedPercent: 74.6, windowMinutes: 300, resetsAt: "2026-09-10T21:52:00.000Z" }
        },
        {
          id: "weekly",
          title: "Weekly · 7d",
          kind: "weekly",
          window: { usedPercent: 41.0, windowMinutes: 10080, resetsAt: "2026-09-13T16:24:00.000Z" }
        }
      ],
      status: "ok",
      source: "web",
      fetchedAt: "2026-09-10T21:24:02.000Z"
    },
    {
      provider: "cursor",
      title: "Cursor",
      account: "user@example.com",
      plan: "Pro",
      windows: [
        {
          id: "weekly",
          title: "Weekly · 7d",
          kind: "weekly",
          window: { usedPercent: 96.8, windowMinutes: 10080, resetsAt: "2026-09-12T06:24:00.000Z" }
        }
      ],
      status: "ok",
      source: "cli",
      fetchedAt: "2026-09-10T21:24:05.000Z"
    },
    {
      provider: "copilot",
      title: "Copilot",
      plan: "Individual",
      windows: [],
      status: "notConfigured",
      error: "No credentials found — connect this provider to start tracking it.",
      source: "mock",
      fetchedAt: "2026-09-10T21:24:10.000Z"
    },
    {
      provider: "gemini",
      title: "Gemini",
      account: "user@example.com",
      plan: "AI Pro",
      windows: [],
      status: "error",
      error: "HTTP 503 from upstream usage endpoint.",
      source: "apiKey",
      fetchedAt: "2026-09-10T21:24:10.000Z"
    },
    {
      provider: "openrouter",
      title: "OpenRouter",
      account: "sk-or-v1…9f2c",
      windows: [],
      status: "error",
      error: "OAuth token expired — sign in again to resume usage reporting.",
      source: "oauth",
      fetchedAt: "2026-09-10T21:24:10.000Z"
    },
    {
      provider: "claude",
      title: "Claude (second account)",
      account: "work@example.org",
      plan: "Team",
      windows: [
        {
          id: "session",
          title: "Session · 5h",
          kind: "session",
          window: { usedPercent: 52.1, windowMinutes: 300, resetsAt: "2026-09-10T22:40:00.000Z" }
        }
      ],
      status: "stale",
      error: "Refresh failed 12 minutes ago.",
      source: "oauth",
      fetchedAt: "2026-09-10T21:12:00.000Z"
    },
    {
      provider: "codex",
      title: "Codex (no usage reported)",
      account: "user@example.com",
      plan: "Business",
      windows: [
        {
          id: "weekly",
          title: "Weekly · 7d",
          kind: "weekly",
          usageKnown: false,
          window: {
            usedPercent: 0,
            windowMinutes: 10080,
            resetDescription: "Resets weekly"
          }
        }
      ],
      status: "ok",
      source: "web",
      fetchedAt: "2026-09-10T21:24:10.000Z"
    }
  ]
};
