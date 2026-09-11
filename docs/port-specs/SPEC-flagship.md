# SPEC — Port a Windows de los 5 proveedores insignia

**Estado:** especificación de port. Fuente de verdad: código Swift de `repo/` (CodexBar de steipete, macOS) — **repo de solo lectura**.
**Objetivo:** app de bandeja Windows (Rust + Tauri) que reutiliza **sesiones locales ya existentes** de otras herramientas (Codex CLI, Claude Code, Cursor, Gemini CLI, GitHub Copilot) **sin login propio** y **sin escribir** en los archivos de credenciales.
**Fecha de validación en vivo:** 2026-09-10 · máquina `%USERPROFILE%` (Windows 11, git-bash).

> Invariante de producto (heredado de CodexBar): el port **lee** credenciales, llama endpoints de uso y **muestra** límites. Nunca escribe tokens, nunca refresca material compartido, nunca hace login por su cuenta. Cuando una credencial nativa está vencida, se delega la recuperación a la CLI dueña del archivo (`codex login`, `claude`, …) y se muestra un error accionable.

---

## 1. Contratos transversales

### 1.1 `RateWindow` (unidad de salida común)

Definición canónica: `repo/Sources/CodexBarCore/UsageFetcher.swift:3`.

```rust
// Modelo Rust de destino
pub struct RateWindow {
    pub used_percent: f64,          // NO se clampea aquí; el display clampa en [0,100]
    pub window_minutes: Option<i64>, // 300 = 5h, 10080 = 7d, None = desconocido
    pub resets_at: Option<SystemTime>,
    pub reset_description: Option<String>, // texto de reset (scrape de Claude CLI)
    pub next_regen_percent: Option<f64>,
    pub is_synthetic_placeholder: bool,    // true = ventana rellenada, no real (Claude web 0% sin sesión)
}
```

Cada proveedor produce hasta 3 ventanas (`primary`, `secondary`, `tertiary`) + `extra_rate_windows[]` (con id/título) + `details[]` + `identity`.

### 1.2 Resolución de rutas home/entorno

Todo path se resuelve desde entorno, nunca hardcodeado:

| Variable | Uso |
|---|---|
| `CODEX_HOME` | home alterno de Codex (aislado; nunca toma prestadas credenciales externas) |
| `CLAUDE_CONFIG_DIR` | raíz literal de configuración de Claude (path único, las comas son parte del valor) |
| `CLAUDE_SECURESTORAGE_CONFIG_DIR` | raíz del store seguro de Claude |
| `XDG_CONFIG_HOME` | base Linux de config (Cursor/CodexBar); en Windows usar `%APPDATA%` |
| `XDG_DATA_HOME` | base Linux de datos (OpenCode) |
| `HOME` | home POSIX; en Windows, `%USERPROFILE%` |
| `GEMINI_OAUTH_CLIENT_ID` / `_SECRET` / `GEMINI_OAUTH2_JS_PATH` | override del cliente OAuth de Gemini |

Regla de expansión (Claude, `ClaudeConfigPaths.swift:75`): solo una barra inicial `/` es absoluta; `~/...` se deja **literal** (no se expande).

---

## 2. Proveedor Codex

### 2.1 Credenciales
| | Ruta |
|---|---|
| Nativo (macOS) | `~/.codex/auth.json` o `$CODEX_HOME/auth.json` |
| Nativo (Windows) | `%USERPROFILE%\.codex\auth.json` o `%CODEX_HOME%\auth.json` |
| Legacy (opt-in) | `~/.config/codex/auth.json` |
| OpenCode (opt-in) | `$XDG_DATA_HOME/opencode/auth.json` · fallback `~/.local/share/opencode/auth.json` |

Los externos (legacy/OpenCode) solo se consideran si el ajuste **External Codex OAuth sources** está activado (por defecto **off**); solo aceptan estructuras OAuth, nunca `OPENAI_API_KEY`. Un `CODEX_HOME` explícito queda aislado.

Estructura (verificada):
```json
{ "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": { "id_token": "...", "access_token": "...", "refresh_token": "...", "account_id": "<uuid>" },
  "last_refresh": "2026-04-04T16:15:31.319Z" }
```
`account_id` se normaliza antes del fallback por JWT.

### 2.2 Frescura / propiedad del token
- La CLI de Codex es la **dueña** de `auth.json` y del refresh (`https://auth.openai.com/oauth/token`). El port **nunca** refresca ni escribe.
- Ventana de refresh: **5 minutos** para credenciales nativas; 60 s para externas.
- Pista de expiración: `exp` del `access_token` (JWT firmado, entero). Rango válido de Chrono `-8334601228800 .. 8210266876799`; booleans/strings/fracciones/floats/exponentes/overflow → caen al fallback por edad (`last_refresh`, regla de 8 días).
- Estados: `nativeRefreshRequired` (nativo vencido → delegar a CLI) · `readOnlySource` (externo vencido → fail-closed).

### 2.3 Endpoints
| Uso | Método / URL |
|---|---|
| Uso (default) | `GET https://chatgpt.com/backend-api/wham/usage` |
| Uso (si `chatgpt_base_url` no contiene `/backend-api`) | `GET {base}/api/codex/usage` |
| Reset-credits (best-effort) | `GET https://chatgpt.com/backend-api/wham/rate-limit-reset-credits` |
| Spend-controls mensual | `GET {base}/accounts/{account_id}/spend-controls/current-user/monthly-usage` |

`chatgpt_base_url` se lee de `config.toml` (`chatgpt_base_url = "..."`, comillas opcionales).

Headers (uso):
```
Authorization: Bearer <access_token>
ChatGPT-Account-Id: <account_id>        # header usa guion final, no se envía si vacío
User-Agent: CodexBar
Accept: application/json
```
Headers (reset-credits): añade `OpenAI-Beta: codex-1`, `originator: Codex Desktop` y `ChatGPT-Account-ID` (mayúsculas).

### 2.4 Respuesta → mapeo
`plan_type` ∈ `guest|free|go|plus|pro|free_workspace|team|business|education|quorum|k12|enterprise|edu`.
`rate_limit.primary_window` → lane **sesión** (5h); `rate_limit.secondary_window` → lane **semanal** (7d). Cada `*_window`: `used_percent` (int), `reset_at` (epoch seg), `limit_window_seconds`.
`credits`: `has_credits`, `unlimited`, `balance`.
`additional_rate_limits[]` → ventanas nombradas (`limit_name`, `metered_feature`) → `extra_rate_windows`.
`individual_limit` / `spend_control.individual_limit`: precedencia root → `rate_limit` → `spend_control`.
Decodificación tolerante: cada ventana se decodifica por separado; un malformado no descarta a sus hermanos.

### 2.5 Vía alternativa (sin OAuth, solo diagnóstico)
RPC JSON sobre stdio lanzando `codex -s read-only -a never app-server` con `initialize` / `account/read` / `account/rateLimits/read`. **Nota de port:** en Windows requiere lanzar el `.exe`/`.cmd` de codex y cerrar stdin→SIGTERM/SIGKILL por timeout; el port debe implementar el ciclo de vida del hijo. No usar como fuente por defecto si OAuth funciona.

---

## 3. Proveedor Claude

### 3.1 Credenciales
| | Ruta |
|---|---|
| Archivo (macOS/Windows) | `~/.claude/.credentials.json` (`%USERPROFILE%\.claude\.credentials.json`) |
| Env | `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR` |
| Keychain (solo macOS) | servicio `Claude Code-credentials` — **sin equivalente Windows**: el port usa solo el archivo |

Estructura (verificada): `claudeAiOauth.{accessToken, refreshToken, expiresAt(ms), refreshTokenExpiresAt(ms), scopes[], subscriptionType, rateLimitTier}`. El archivo también contiene `mcpOAuth.*` (estado OAuth de servidores MCP, **ignorar** para uso).
Alcance requerido: `user:profile` (un token solo-`user:inference` no puede consultar uso). `Claude Code 2.1.x` puede dejar el item de Keychain solo con `mcpOAuth` → es un error de configuración OAuth, no un fallo de red.

### 3.2 Endpoints
| Uso | Método / URL |
|---|---|
| Uso (preferido) | `GET https://api.anthropic.com/api/oauth/usage` |
| Perfil | `GET https://api.anthropic.com/api/oauth/profile` |
| Admin API (si hay `sk-ant-admin…`) | `GET/POST /v1/organizations/cost_report` · `/v1/organizations/usage_report/messages` |
| Web (cookies `sessionKey`) | `claude.ai/api/organizations` → `{orgId}` → `/usage`, `/overage_spend_limit`, `/prepaid/credits`, `/api/account` |

Headers (uso OAuth):
```
Authorization: Bearer <accessToken>
anthropic-beta: oauth-2025-04-20      # obligatorio para el endpoint de uso
Accept: application/json
Content-Type: application/json
User-Agent: claude-code/<version>     # detecta versión de la CLI; fallback 2.1.0
```
Errores: 401 → reautenticar · 429 → `rateLimited` con `Retry-After` (gate propio) · 403 → error terminal de permisos. **403 no** dispara fallback automático.

### 3.3 Respuesta → mapeo
`five_hour` → sesión · `seven_day` → semana (y fallback primario si falta `five_hour`) · `seven_day_sonnet`/`seven_day_opus` → semana por modelo · `limits[].weekly_scoped` → ventanas semanales por modelo (`scope.model.display_name`, p.ej. "Fable"; `All models` queda en la semana principal) · `seven_day_routines`/`seven_day_cowork` → ventana extra "Daily Routines" · `extra_usage` → gasto/límite mensual.
Forma de ventana: `utilization` (0–100), `resets_at` (ISO-8601).
Plan: `subscriptionType` preferido; fallback `rate_limit_tier` (`default_claude_max_5x` → "Max 5x", `…_20x` → "Max 20x").
Web: un `five_hour: null` con semana real produce una ventana `0%` **sintética** (`is_synthetic_placeholder: true`) que **no** debe mostrarse como sesión real.

### 3.4 Orden de selección
App: `OAuth → CLI PTY → Web`. CLI runtime: `Web → CLI PTY`. Los modos explícitos no hacen fallback.
CLI PTY (fallback): lanza `claude --allowed-tools ""`, responde prompts de primer arranque, envía `/usage`, parsea "Current session"/"Current week". En Windows requiere PTY real (ConPTY); **costoso** — dejarlo como opción explícita, no por defecto.

---

## 4. Proveedor Cursor

> En esta máquina **no hay** Cursor instalado (ver §8): rutas y endpoints son teóricos, marcados **no verificables aquí**.

### 4.1 Fuentes y orden
1. **Auth local de Cursor.app** (preferido en Auto): DB VS-Code `ItemTable` clave `cursorAuth/accessToken`.
   | | Ruta de la DB |
   |---|---|
   | macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
   | Linux | `$XDG_CONFIG_HOME/Cursor/User/globalStorage/state.vscdb` (fallback `~/.config/...`) |
   | **Windows (teórico)** | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` |
   En el Swift, `resolveDefaultDBPath` devuelve `""` para plataformas distintas de macOS/Linux — **el port debe añadir la rama Windows**. Sidecars activos: `state.vscdb-wal`, `state.vscdb-shm`. Abrir **read-only**; con WAL activo leer normal; WAL inactivo sin sidecars → modo `immutable=1` (no recrear archivos en el directorio de Cursor).
   Token es JWT: cuenta solo si `exp` está a >60 s. Cookie derivada `WorkosCursorSessionToken=<userId>%3A%3A<token>`. Decodificación BLOB: reconocer ASCII UTF-16LE sin BOM antes de UTF-8 (evita bytes NUL intercalados).
2. **Cookie header cacheado** (tras import exitoso).
3. **Import de cookies de navegador**: dominios `cursor.com`, `cursor.sh`; nombres `WorkosCursorSessionToken`, `__Secure-next-auth.session-token`, `next-auth.session-token`.
4. **Sesión almacenada** en `<app-support>/cursor-session.json` (fallback legacy).

### 4.2 Endpoints
| Uso | Método / URL |
|---|---|
| Resumen de plan | `GET https://cursor.com/api/usage-summary` |
| Identidad | `GET https://cursor.com/api/auth/me` |
| Uso legacy por requests | `GET https://cursor.com/api/usage?user=<id>` |
| Grok Bot semanal | `POST https://cursor.com/api/dashboard/get-sand-usage-status` (requiere `Origin: https://cursor.com`) |
| Coste (opt-in) | `POST https://cursor.com/api/dashboard/get-filtered-usage-events` (requiere `Origin` para CSRF) |

### 4.3 Mapeo
Primary: plan usage % (incluido). Secondary: modelos Cursor %. Tertiary: Third Party %. Extra: Grok Bot semanal (`usagePercent`, `nextResetTimestampUtc`) si allowance ≠ 0. Reset: fin de ciclo mensual (Grok Bot usa `nextResetTimestampUtc`).
**Port nota:** `usage-summary` devuelve enums con variantes `used`/`limit`/`remaining`/`breakdown` (`included`/`bonus`/`total`) y `autoPercentUsed`/`apiPercentUsed`/`totalPercentUsed`; decodificar con `Option` para tolerar esquemas cambiantes.

---

## 5. Proveedor Gemini

> Sin credenciales en esta máquina (ver §8): **no verificable aquí**.

### 5.1 Credenciales
| | Ruta |
|---|---|
| Credenciales | `~/.gemini/oauth_creds.json` (Windows: `%USERPROFILE%\.gemini\oauth_creds.json`) |
| Ajuste de auth | `~/.gemini/settings.json` → `security.auth.selectedType` |
| Cliente OAuth (id/secret) | extraído de `oauth2.js` de la CLI instalada |

Campos requeridos de `oauth_creds.json`: `access_token`, `refresh_token` (opcional), `id_token`, `expiry_date`.
Tipos de auth: `oauth-personal` (o desconocido → intentar OAuth). `api-key` y `vertex-ai` → **error duro** (fuera de alcance).

Extracción del cliente OAuth (orden):
1. `GEMINI_OAUTH_CLIENT_ID` + `GEMINI_OAUTH_CLIENT_SECRET`.
2. `GEMINI_OAUTH2_JS_PATH` → `oauth2.js` legible.
3. `oauth2.js` del paquete instalado / regex sobre bundle.
4. Rutas conocidas de Homebrew (macOS). **Port Windows:** buscar bajo el prefijo npm global, p.ej. `%APPDATA%\npm\node_modules\@google\gemini-cli-core\dist\src\code_assist\oauth2.js` y variantes de `@google/gemini-cli/node_modules/...`; regex `OAUTH_CLIENT_ID` / `OAUTH_CLIENT_SECRET`.

### 5.2 Endpoints
| Uso | Método / URL |
|---|---|
| Cuota | `POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota` (body `{"project":"<id>"}` o `{}`) |
| Tier | `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` (body `{"metadata":{"ideType":"GEMINI_CLI","pluginType":"GEMINI"}}`) |
| Proyecto (fallback) | `GET https://cloudresourcemanager.googleapis.com/v1/projects` (elegir `gen-lang-client*` o label `generative-language`) |
| Refresh token | `POST https://oauth2.googleapis.com/token` (form: `client_id`, `client_secret`, `refresh_token`, `grant_type=refresh_token`) |

Header cuota: `Authorization: Bearer <access_token>`.

### 5.3 Mapeo y tiers
Buckets: `remainingFraction`, `resetTime`, `modelId`; por modelo gana el menor `remainingFraction`; `percentLeft = remainingFraction*100`. Primary: modelos Pro (menor % restante). Secondary: modelos Flash.
Tier (`loadCodeAssist`): `paidTier.name` (preferido) → `standard-tier`="Paid" → `free-tier`+claim `hd`="Workspace" → `free-tier`="Free" → `legacy-tier`="Legacy". Email desde claims del `id_token`.
**Migración consumer (2026-06-18):** Google dejó de servir OAuth Gemini CLI a cuentas individual/AI Pro/Ultra. Señales: `UNSUPPORTED_CLIENT`, `IneligibleTierError`, o cuerpo 200 de `loadCodeAssist` sin `currentTier` con `ineligibleTiers[].reasonCode == "UNSUPPORTED_CLIENT"` y `retrieveUserQuota` 403 `SUBSCRIPTION_REQUIRED`. Un 403 solo se mapea a `consumerTierDeprecated` si esa misma llamada vio la señal **y** la cuenta no es `standard-tier`. `paidTier.name` o claim `hd` suprimen la señal.

---

## 6. Proveedor GitHub Copilot

> Sin credenciales en esta máquina (ver §8): **no verificable aquí**. Nótese que este proveedor **no lee** `~/.config/github-copilot`: el token lo gestiona la propia app.

### 6.1 Token (device flow, iniciado por el usuario)
| Paso | Método / URL |
|---|---|
| Device code | `POST https://github.com/login/device/code` |
| Poll de token | `POST https://github.com/login/oauth/access_token` |

- `client_id` = `Iv1.b507a08c87ecfe98` (VS Code), `scope` = `read:user`, `grant_type=urn:ietf:params:oauth:grant-type:device_code`.
- Poll: `authorization_pending` → seguir; `slow_down` → +5 s; `expired_token` → timeout; otro error → fallo auth.
- Host empresarial opcional (`enterpriseHost` en config o UI): se normaliza `https://octocorp.ghe.com/login` → `octocorp.ghe.com`; rutas `https://<host>/login/...`; identidad `https://api.<host>/user`; puerto ≠443 se conserva; puntos finales se normalizan.
- Token almacenado en la config del port (no en un archivo de terceros), por proveedor/`tokenAccounts`.

### 6.2 Uso
| Uso | Método / URL |
|---|---|
| Uso | `GET https://api.github.com/copilot_internal/user` (o `https://api.<enterpriseHost>/copilot_internal/user`) |
| Identidad | `GET https://api.github.com/user` |

Headers:
```
Authorization: token <github_oauth_token>   # el token OAuth de GitHub, NO el de Copilot
Accept: application/json
Editor-Version: vscode/1.96.2
Editor-Plugin-Version: copilot-chat/0.26.7
User-Agent: GitHubCopilotChat/0.26.7
X-Github-Api-Version: 2025-04-01
```
401/403 → `userAuthenticationRequired`.

### 6.3 Mapeo
Primary: `quotaSnapshots.premiumInteractions` % restante → usado. Secondary: `quotaSnapshots.chat`. Si solo hay `chat`, va al slot secondary (mantener etiquetas "Premium"/"Chat"). Billing por tokens o `unlimited` → sin barras falsas. Plan: `copilotPlan`. **La API no da fecha de reset.**
Budget extras (opt-in, solo host público): `GET https://github.com/settings/billing/budgets?page=<p>&page_size=10&scope=customer` con cookies de `github.com`, `Accept: application/json`, `X-Requested-With: XMLHttpRequest`, `GitHub-Verified-Fetch: true`, `X-Fetch-Nonce: <nonce fresco>`. Es un endpoint web, no la REST pública.

---

## 7. Tabla resumen de rutas Windows

| Proveedor | macOS | **Windows (destino)** | Token de terceros leído |
|---|---|---|---|
| Codex | `~/.codex/auth.json` | `%USERPROFILE%\.codex\auth.json` | sí (Codex CLI) |
| Claude | `~/.claude/.credentials.json` (+Keychain) | `%USERPROFILE%\.claude\.credentials.json` | sí (Claude Code) |
| Cursor | `~/Library/Application Support/Cursor/…/state.vscdb` | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` | sí (Cursor.app) |
| Gemini | `~/.gemini/oauth_creds.json` | `%USERPROFILE%\.gemini\oauth_creds.json` | sí (Gemini CLI) |
| Copilot | — | — (token propio, device flow) | no |

---

## 8. Verificación EN VIVO (esta máquina, 2026-09-10)

Método: lectura de archivos de credenciales y llamadas `GET` a los endpoints de uso con los tokens ya presentes. **No se imprimieron, copiaron ni guardaron tokens; no se escribió ningún archivo de credenciales; no se hizo login; no se modificó el repo.**

### 8.1 Presencia de credenciales
| Objetivo | Ruta | Resultado |
|---|---|---|
| Codex | `%USERPROFILE%\.codex\auth.json` | **PRESENTE** (4398 B) — `auth_mode: chatgpt`, `tokens.access_token`, `tokens.account_id`, `tokens.refresh_token`, `last_refresh: 2026-04-04T16:15:31Z` |
| Claude | `%USERPROFILE%\.claude\.credentials.json` | **PRESENTE** (19827 B) — `claudeAiOauth` con `accessToken`, `refreshToken`, `expiresAt`, scopes incl. `user:profile`, `subscriptionType: max`, `rateLimitTier: default_claude_max_20x` |
| Cursor | `%APPDATA%\Cursor\...\state.vscdb`, `~/.cursor` | **AUSENTE** |
| Gemini | `~/.gemini/oauth_creds.json`, `~/.gemini/settings.json` | **AUSENTE** (`~/.gemini` existe pero solo con datos de Antigravity) |
| Copilot | `~/.config/github-copilot`, `%APPDATA%\github-copilot` | **AUSENTE** (el proveedor de todos modos no lee estos archivos) |

### 8.2 Resultado de las llamadas en vivo
| Proveedor | Petición | HTTP | Cuerpo (resumido) |
|---|---|---|---|
| Codex | `GET https://chatgpt.com/backend-api/wham/usage` | **401** | `{"error":{"code":"unauthorized_unknown","message":"Could not parse your authentication token. Please try signing in again."},"status":401}` (0.50 s) |
| Codex | `GET .../wham/rate-limit-reset-credits` | **401** | mismo envelope (0.33 s) |
| Claude | `GET https://api.anthropic.com/api/oauth/usage` | **401** | `{"type":"error","error":{"type":"authentication_error","message":"OAuth access token has been revoked."}}` (0.26 s) |
| Claude | `GET https://api.anthropic.com/api/oauth/profile` | **401** | mismo `authentication_error` (0.23 s) |

**Diagnóstico (evidencia de disco, sin tokens):**
- **Codex:** `exp` del `access_token` ≈ 1776183328 → **vencido hace ~3.580 h (~149 días)**; `last_refresh` 2026-04-04. Comportamiento correcto del diseño: credencial nativa vencida → `nativeRefreshRequired` → delegar a `codex login`. El port **no** debe refrescar en proceso.
- **Claude:** `expiresAt` **vencido hace ~23,6 h**; `refreshTokenExpiresAt` aún válido (+443 h). El 401 `revoked` obliga a re-login en Claude Code; el port no debe intentar refresh.

**Conclusión:** DNS + TLS + ruta + método + headers ejercitados y respondidos por el servidor → los endpoints y contratos quedan validados como vivos y correctos. **No fue posible obtener datos de uso reales** porque los tokens locales están vencidos/revocados, y refrescarlos está prohibido por la restricción de solo-lectura. La verificación con datos reales queda pendiente de `codex login` / `claude` por parte del usuario.

---

## 9. Criterios de aceptación del port

1. Resolver rutas por entorno con las equivalencias Windows de §7; nunca hardcodear `~/`.
2. Leer credenciales en modo solo-lectura, sin locks de escritura, sin crear sidecars.
3. Rechazar (fail-closed) cuando el token esté vencido según la regla del proveedor; mostrar acción de re-login de la CLI dueña, **sin** refrescar ni escribir.
4. Producir `RateWindow`s con `used_percent` sin clampear y `is_synthetic_placeholder` correcto.
5. Decodificación tolerante por ventana (un campo malformado no descarta el resto).
6. Copilot: device flow explícito del usuario; nunca leer `~/.config/github-copilot`.
7. Reimplementar en Windows el fallback que el Swift devuelve vacío: `Cursor` (rama Windows) y localización de `oauth2.js` npm para Gemini.
8. Logs y errores nunca contienen tokens; los cuerpos de error se truncan a 400 chars.

## 10. Gotchas principales
Ver `top_gotchas` en el contrato JSON de salida.
