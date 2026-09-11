// AUTO-GENERATED fixture — do not edit by hand.
// Regenerate: cargo run -p codexbar-cli -- usage --format json > evidence/usage-mock.json
//             then node scripts/gen-ui-mock.mjs
// Identical to the payload the Tauri backend serves, schemaVersion 1.
window.__CODEXBAR_MOCK__ = {
  "schemaVersion": 1,
  "generatedAt": "2026-09-11T01:28:20.199Z",
  "providers": [
    {
      "provider": "codex",
      "title": "Codex",
      "account": "user@example.com",
      "plan": "Plus",
      "windows": [
        {
          "id": "session",
          "title": "Session · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 33,
            "windowMinutes": 300,
            "resetsAt": "2026-09-11T03:38:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · 7d",
          "kind": "weekly",
          "window": {
            "usedPercent": 62.8,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-16T21:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "claude",
      "title": "Claude",
      "account": "user@example.com",
      "plan": "Max 20x",
      "windows": [
        {
          "id": "session",
          "title": "Session · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 54.3,
            "windowMinutes": 300,
            "resetsAt": "2026-09-11T04:04:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · 7d",
          "kind": "weekly",
          "window": {
            "usedPercent": 49.6,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-15T12:28:20.199Z"
          }
        },
        {
          "id": "weekly-sonnet",
          "title": "Weekly · Sonnet",
          "kind": "weeklyScoped",
          "window": {
            "usedPercent": 34.7,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-15T12:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "cursor",
      "title": "Cursor",
      "account": "user@example.com",
      "plan": "Pro",
      "windows": [
        {
          "id": "session",
          "title": "Session · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 90.9,
            "windowMinutes": 300,
            "resetsAt": "2026-09-11T04:52:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · 7d",
          "kind": "weekly",
          "window": {
            "usedPercent": 97,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-16T17:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "openrouter",
      "title": "OpenRouter",
      "account": "sk-or-v1…9f2c",
      "windows": [
        {
          "id": "credits",
          "title": "Credits · 30d",
          "kind": "extra",
          "window": {
            "usedPercent": 52.2,
            "windowMinutes": 43200,
            "resetsAt": "2026-09-16T11:28:20.199Z"
          }
        }
      ],
      "balance": {
        "amount": 32.1,
        "currency": "USD",
        "label": "Credits remaining"
      },
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "copilot",
      "title": "Copilot",
      "plan": "Individual",
      "windows": [],
      "status": "notConfigured",
      "error": "No credentials found — open Settings to connect this provider.",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "gemini",
      "title": "Gemini",
      "account": "user@example.com",
      "plan": "AI Pro",
      "windows": [],
      "status": "error",
      "error": "HTTP 503 from upstream usage endpoint (mock failure).",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "deepseek",
      "title": "DeepSeek",
      "windows": [],
      "balance": {
        "amount": 17.5,
        "currency": "USD",
        "label": "Balance"
      },
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "groq",
      "title": "Groq",
      "account": "user@example.com",
      "plan": "Enterprise",
      "windows": [
        {
          "id": "daily",
          "title": "Daily · tokens",
          "kind": "extra",
          "window": {
            "usedPercent": 49.1,
            "windowMinutes": 1440,
            "resetsAt": "2026-09-16T13:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "zai",
      "title": "z.ai",
      "account": "user@example.com",
      "plan": "Coding Plan Pro",
      "windows": [
        {
          "id": "session",
          "title": "Session · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 25.9,
            "windowMinutes": 300,
            "resetsAt": "2026-09-11T05:10:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · 7d",
          "kind": "weekly",
          "window": {
            "usedPercent": 74.9,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-14T19:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "minimax",
      "title": "MiniMax",
      "plan": "Coding Plan",
      "windows": [],
      "status": "notConfigured",
      "error": "No credentials found — open Settings to connect this provider.",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "kimi",
      "title": "Kimi",
      "account": "user@example.com",
      "plan": "Moderato",
      "windows": [
        {
          "id": "rate-limit",
          "title": "Rate limit · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 47,
            "windowMinutes": 300,
            "resetsAt": "2026-09-11T01:50:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · plan",
          "kind": "weekly",
          "window": {
            "usedPercent": 57.3,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-13T19:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "elevenlabs",
      "title": "ElevenLabs",
      "plan": "Creator",
      "windows": [
        {
          "id": "characters",
          "title": "Characters · monthly",
          "kind": "weekly",
          "window": {
            "usedPercent": 23.5,
            "windowMinutes": 43200,
            "resetsAt": "2026-09-15T11:28:20.199Z"
          }
        },
        {
          "id": "voice-slots",
          "title": "Voice slots",
          "kind": "extra",
          "window": {
            "usedPercent": 64.2
          }
        },
        {
          "id": "professional-voices",
          "title": "Professional voices",
          "kind": "extra",
          "window": {
            "usedPercent": 50.2
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "xai",
      "title": "xAI",
      "windows": [
        {
          "id": "prepaid",
          "title": "Prepaid · 30d",
          "kind": "extra",
          "window": {
            "usedPercent": 22.4,
            "windowMinutes": 43200,
            "resetsAt": "2026-09-14T01:28:20.199Z"
          }
        }
      ],
      "balance": {
        "amount": 117.8,
        "currency": "USD",
        "label": "Prepaid balance"
      },
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    },
    {
      "provider": "opencodego",
      "title": "OpenCode Go",
      "account": "wrk_example…0001",
      "windows": [
        {
          "id": "session",
          "title": "Session · 5h",
          "kind": "session",
          "window": {
            "usedPercent": 38.5,
            "windowMinutes": 300,
            "resetsAt": "2026-09-15T22:28:20.199Z"
          }
        },
        {
          "id": "weekly",
          "title": "Weekly · 7d",
          "kind": "weekly",
          "window": {
            "usedPercent": 52.6,
            "windowMinutes": 10080,
            "resetsAt": "2026-09-15T22:28:20.199Z"
          }
        },
        {
          "id": "monthly",
          "title": "Monthly · 30d",
          "kind": "extra",
          "window": {
            "usedPercent": 15.7,
            "windowMinutes": 43200,
            "resetsAt": "2026-09-15T22:28:20.199Z"
          }
        }
      ],
      "status": "ok",
      "source": "mock",
      "fetchedAt": "2026-09-11T01:28:20.199Z"
    }
  ]
};
