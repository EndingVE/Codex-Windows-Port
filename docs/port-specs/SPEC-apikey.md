# SPEC — Port a Windows de los 9 proveedores de API key / token local

**Estado:** especificación de port. Fuente de verdad: código Swift de `repo/` (CodexBar de steipete, macOS) — **repo de solo lectura, no modificado**.
**Objetivo:** app de bandeja Windows (Rust + Tauri) que replica el comportamiento de CodexBar para 9 proveedores que se autentican con **API key o token local existente**, sin login propio.
**Proveedores cubiertos:** OpenRouter · DeepSeek · Groq · z.ai (GLM) · MiniMax · Kimi (Moonshot / Kimi Open Platform) · ElevenLabs · xAI (Grok platform) · OpenCode Go.
**Fecha de validación en vivo:** 2026-09-10 · máquina `%USERPROFILE%` (Windows 11, git-bash).
**Documento hermano:** `SPEC-flagship.md` cubre Codex/Claude/Cursor/Gemini/Copilot (no solapar).

> **Invariante de producto (heredado de CodexBar):** el port **lee** credenciales locales ya existentes, llama endpoints de uso/límites y **muestra** el resultado. Nunca escribe tokens, nunca refresca material compartido (refresh tokens, cookies de sesión ajenas), nunca hace login por su cuenta. Cuando una credencial está vencida, se delega la recuperación a la herramienta dueña del archivo (`opencode auth login`, `kimi`, login del navegador, …) y se muestra un error accionable.

**Legend de verificación (usado en todo el documento):**
- ✅ **VERIFICADO EN VIVO** en esta máquina Windows (path/archivo/schema/consulta comprobados; sin imprimir valores secretos).
- 🟡 **LEÍDO DEL CÓDIGO** — lógica y endpoints extraídos del Swift/JS del repo; no hay credencial en esta máquina, no se pudo ejecutar contra la API real.
- ⚪ **NO VERIFICABLE AQUÍ** — requiere un token real o un host no presente; documentado para implementación y prueba posterior.

Ninguna clave de API real se leyó, imprimió ni guardó al escribir este documento. Los únicos valores inspeccionados fueron **nombres de variables de entorno, nombres de archivo, esquema SQLite y claves de primer nivel de JSON** (sin sus valores).

---

## 1. Contratos transversales

### 1.1 Modelo de credenciales y precedencia

Todos estos proveedores comparten el mismo modelo: CodexBar no tiene login propio; resuelve un secreto existente y lo usa como `Authorization`/header de API. El orden efectivo de precedencia, común a los 9, es:

1. **Cuenta de token activa** (`tokenAccounts[activeIndex]` en el config de CodexBar) — se inyecta en el entorno antes de leer; el valor de entorno del proveedor se "scrubbea" para que una clave vieja del entorno no enmascare la cuenta seleccionada.
2. **`providers[].apiKey`** del config de CodexBar (más campos auxiliares: `workspaceID`, `region`, `cookieHeader`).
3. **Variable de entorno del SO** (cada proveedor tiene su nombre; §1.3).
4. **(Solo algunos)** archivo local de la propia CLI/agente o base de datos de navegador.

Referencia: `repo/Sources/CodexBarCore/Providers/ProviderEnvironmentResolver.swift`, `repo/Sources/CodexBarCore/Config/CodexBarConfigStore.swift`.

### 1.2 Limpieza canónica de un valor de credencial

Todos los `*SettingsReader.swift` aplican la misma regla `cleaned()` antes de aceptar un valor. El port Rust debe replicarla:

1. `trim()` de espacios y saltos de línea; si queda vacío → `None`.
2. Si el valor está envuelto en comillas dobles **o** simples coincidentes (`"..."` / `'...'`) → se quitan una vez.
3. Segundo `trim()`. Si queda vacío → `None`.

Esto importa porque en Windows es común pegar valores como `setx FOO "\"sk-...\""` o desde archivos `.env`.

### 1.3 Variables de entorno en Windows

- Windows usa variables de entorno **case-insensitive**: `OPENROUTER_API_KEY`, `OpenRouter_Api_Key` y `openrouter_api_key` deben tratarse como equivalentes. El port Rust debe leer el entorno con comparación case-insensitive (o normalizar todas las claves a mayúsculas al construir el mapa de entorno).
- `HOME`/`~` ⇒ `%USERPROFILE%` (p. ej. `%USERPROFILE%`). `XDG_DATA_HOME` ⇒ si no está, la convención que usan las CLIs es literalmente `%USERPROFILE%\.local\share` (ver §2 / OpenCode Go ✅).
- Una app de bandeja lanzada al inicio de sesión hereda el entorno de usuario. Si el usuario define la variable después con `setx`, requiere reinicio de sesión; la UI debe ofrecer "relanzar" o leer del registro de usuario (`HKCU\Environment`) además de `GetEnvironmentVariableW`.

### 1.4 Config propia del port (dónde persistir selección/estado)

CodexBar guarda su propio estado en:
- Instalaciones nuevas: `~/.config/codexbar/config.json`
- Legacy: `~/.codexbar/config.json`
- Override: variable `CODEXBAR_CONFIG=<path>` (`CodexBarConfigStore.pathEnvironmentKey`).

**En Windows (decisión para el port):** persistir en `%APPDATA%\codexbar\config.json` (Roaming), aceptando override por `CODEXBAR_CONFIG`. El puerto **no** debe reescribir los `config.json` de las CLIs de terceros. Esquema mínimo a emular:

```jsonc
{
  "providers": [
    { "id": "openrouter", "enabled": true, "apiKey": "***" },
    { "id": "xai", "enabled": true, "apiKey": "***", "workspaceID": "<XAI_TEAM_ID>" },
    { "id": "zai", "enabled": true, "region": "bigmodel-cn",
      "tokenAccounts": { "version": 1, "activeIndex": 0,
        "accounts": [ { "id": "<uuid>", "label": "Team", "token": "***",
          "usageScope": "team", "organizationId": "org_...", "workspaceID": "proj_..." } ] } }
  ]
}
```
`tokenAccounts` está definido en `repo/Sources/CodexBarCore/Config/CodexBarConfig.swift`.

### 1.5 Validación de overrides de endpoint (política de seguridad)

Varios proveedores permiten override de host/URL. Regla común (`ProviderEndpointOverrideValidator`):
- El override debe normalizarse a **HTTPS** (o un host/bare-path que CodexBar normaliza a HTTPS). `http://` **falla cerrado antes** de adjuntar el bearer.
- Sin `userinfo` en la URL; sin delimitadores de host codificados.
- Algunos proveedores restringen a hosts propios (MiniMax: sufijos `minimax.io` / `minimaxi.com` con modo estricto opcional; z.ai: chequeo de región contra hosts conocidos).
- **El puerto Windows debe mantener esta política.** Un override `http://` nunca debe recibir la clave.

### 1.6 Importación de cookies de navegador (solo donde aplica)

Kimi, MiniMax, Groq (console) y OpenCode Go (modo web) usan cookies de navegador como **fuente adicional/opcional**. En macOS CodexBar usa `SweetCookieKit` con orden de importación por proveedor y prompts de Keychain. Notas para Windows:

- Los navegadores Chromium guardan las cookies en `%LOCALAPPDATA%\<Vendor>\<App>\User Data\<Profile>\Network\Cookies` (SQLite; esquema `cookies` con `host_key`, `name`, `value`, `encrypted_value`, `path`, `expires_utc`). Los valores están cifrados con **DPAPI (por usuario) + AES-GCM** usando la clave de `...\User Data\Local State` → `os_crypt.encrypted_key` (base64, prefijo `"DPAPI"`). El port Rust debe: leer `Local State`, DPAPI-descifrar la clave con `CryptUnprotectData`, y AES-256-GCM descifrar cada `encrypted_value` (prefijo `v10`/`v11`).
- Firefox: SQLite `%APPDATA%\Mozilla\Firefox\Profiles\<profile>\cookies.sqlite` (sin cifrado), tabla `moz_cookies`.
- Navegadores/estado presentes en esta máquina (✅ existencia comprobada): Chrome (`%LOCALAPPDATA%\Google\Chrome\User Data`), Edge (`%LOCALAPPDATA%\Microsoft\Edge\User Data`), Firefox (`%APPDATA%\Mozilla\Firefox\Profiles`).
- En Windows **no hay "Full Disk Access" ni Keychain**: los errores de permiso de macOS se traducen a "no se pudo descifrar la cookie (perfil bloqueado por el navegador en ejecución)".
- Política heredada: en `Auto`, intentar solo el/los navegador(es) preferidos del proveedor para no leer de más; `Manual` = pegar header `Cookie:` o "Copy as cURL"; `Off` = nunca tocar navegadores. Enum `ProviderCookieSource = { auto, manual, off }`.

### 1.7 Contrato de salida (resumen)

Cada proveedor produce `identity` (login method / plan / balance), `primary`/`secondary`/`tertiary` (`RateWindow`) y `details[]` con filas `{label, value, secondaryValue?, chart?}`. Los proveedores **solo saldo** (DeepSeek API key, Moonshot, xAI prepaid) no sintetizan ventanas de cuota: exponen el saldo en `cost`/identidad. Ver `repo/Sources/CodexBarCore/UsageFetcher.swift` y `SPEC-flagship.md §1.1`.

---

## 2. OpenCode Go ✅/🟡

El único de los 9 con **datos locales verificables en esta máquina**.

### 2.1 Credenciales y fuentes
| Fuente | Ruta / variable |
|---|---|
| API key (autoritativa para cuota) | `OPENCODE_API_KEY` o `providers[].apiKey` (id `opencodego`) |
| Base local (costo/uso por dispositivo) | `%USERPROFILE%\.local\share\opencode\opencode.db` ✅ existe (516 096 bytes) |
| Auth local del CLI | `%USERPROFILE%\.local\share\opencode\auth.json`, entrada `opencode-go.key` ✅ existe |
| Cookies web (modo web) | dominios `opencode.ai`, `app.opencode.ai` |
| Override de workspace | `CODEXBAR_OPENCODE_WORKSPACE_ID` (raw `wrk_…` o URL `https://opencode.ai/workspace/...`) |

> Nota de path: CodexBar resuelve `$HOME/.local/share/opencode` (macOS/Linux). En Windows la convención de OpenCode es la misma forma XDG pero **sin** `%APPDATA%`/`%LOCALAPPDATA%`: `%USERPROFILE%\.local\share\opencode` ✅ comprobado (`opencode.db` y `auth.json` presentes con esa ruta exacta).

### 2.2 Endpoints
| Uso | Método / URL | Auth |
|---|---|---|
| Cuota (API autoritativa) | `GET https://opencode.ai/zen/go/v1/usage` | `Authorization: Bearer <OPENCODE_API_KEY>` |
| Web (fallback) | `POST https://opencode.ai/_server` con server functions `workspaces` (`def39973…0234f`) y `subscription.get` (`7abeebee…91b4`) | cookie de sesión |
| Balance Zen | scrape de `https://opencode.ai/workspace/{id}` (dashboard HTML) | cookie |

Respuesta de `/zen/go/v1/usage`: `rollingUsage.usagePercent`+`resetInSec`, `weeklyUsage.usagePercent`+`resetInSec`, `monthlyUsage`. **Unidades:** son porcentajes 0–100 (`1` = 1%, `0.5` = 0.5%). Resets = `now + resetInSec`.

### 2.3 Lector local (SQLite) — ✅ verificado en Windows
- Base de datos en **WAL**; abrir **read-only** con `busy_timeout=250ms`. Si falla con `SQLITE_CANTOPEN` y **no** existen `-wal`/`-shm`, reintentar en modo inmutable (`file:...?immutable=1`) para no recrear sidecars.
- Si existe tabla `part` → usar el SQL con `step-finish`; si no → SQL de solo `message`.
- Predicado exacto (probado contra `opencode.db` de esta máquina → **4 filas** `opencode-go` assistant):
  ```sql
  -- variant message-only
  SELECT CAST(COALESCE(json_extract(data,'$.time.created'), time_created) AS INTEGER) AS createdMs,
         CAST(json_extract(data,'$.cost') AS REAL) AS cost,
         COALESCE(json_extract(data,'$.modelID'),'') AS modelID
  FROM message
  WHERE json_valid(data)
    AND json_extract(data,'$.providerID') = 'opencode-go'
    AND json_extract(data,'$.role') = 'assistant'
    AND json_type(data,'$.cost') IN ('integer','real');
  ```
  Variante con `part`: suma `part.data.type='step-finish'` con costo; si un mensaje con costo no tiene part `step-finish`, cuenta el mensaje una vez. Agrupar por `CAST(COALESCE(json_extract(p.data,'$.time.created'), p.time_created, m.createdMs) AS INTEGER)`.
- Esquema ✅ observado: `message(id, session_id, time_created, time_updated, data)`, `part(id, message_id, session_id, time_created, time_updated, data)`; `data` es JSON con `providerID`, `role`, `cost`, `modelID`, `time.created`.
- Auth local ✅: `auth.json` contiene `{"opencode-go": {"key": "...", "type": "..."}}`; el lector solo necesita que `opencode-go.key` sea no vacío (no lo usa como credencial de red, solo como señal de "OpenCode Go usado localmente").

### 2.4 Agregación y límites locales
`OpenCodeGoLocalUsageReader` (constantes exactas):
- `session = 12.0 USD` (ventana rodante de 5 h: `now-5h … now`), `weekly = 30.0 USD` (semana UTC empezando **lunes**), `monthly = 60.0 USD` (mes anclado al **día/hora del registro más antiguo**, estimado — puede desviarse del ciclo real).
- `percent = round(clamp(used/limit*100, 0, 100) * 10)/10`.
- `rollingResetInSec = max(0, (oldestSessionMs + 5h - now)/1000)`; weekly reset = fin de semana UTC; monthly reset = fin de mes anclado.
- Historial diario (costos por día **local**, no UTC) con desglose por `modelID`; vacío → bucket `"unknown"`.

### 2.5 Selección de fuente (`sourceModes: [auto, api, web]`)
- `api` → solo `/zen/go/v1/usage` (requiere key).
- `web` → solo web (cookie/manual). Nunca lee la DB local.
- `auto` **sin** token account / cookie manual / workspace override → local primero; si hay key, **superpone** las ventanas autoritativas de la API sobre las locales; si no hay local, cae a API y luego web. Con token account / cookie manual / override de workspace → **web-first** (porque el historial local es de todo el dispositivo, no del workspace).
- Cuando la cuota es solo local (sin overlay autoritativo) se marca `dataConfidence: "estimated"` y no se muestra pace/reserve/run-out.
- Detalle de implementación: `repo/Sources/CodexBarCore/Providers/OpenCodeGo/` (`OpenCodeGoLocalUsageReader.swift`, `OpenCodeGoUsageFetcher.swift`, `OpenCodeGoZenBalanceParser.swift`, `OpenCodeGoProviderDescriptor.swift`). Doc: `repo/docs/opencode.md`.

---

## 3. Kimi (Kimi Code) + Moonshot / Kimi Open Platform 🟡/⚪

Son **dos superficies de facturación distintas** que el port debe tratar como proveedores separados. La tarea agrupa "Kimi (Moonshot)"; en el código son `Providers/Kimi/` (Kimi Code, suscripción) y `Providers/Moonshot/` (Open Platform, saldo por API key).

### 3A. Kimi Code (suscripción `kimi.com/code`)

**Fuentes y prioridad (orden exacto del código, `docs/kimi.md` §Authentication Priority):**
1. **API key** — `providers[].apiKey` (provider `kimi`) o `KIMI_CODE_API_KEY`.
2. **Token de la CLI Kimi Code** — `%USERPROFILE%\.kimi-code\credentials\kimi-code.json` (macOS/Linux: `~/.kimi-code/...`). Estructura `{access_token, refresh_token, expires_at}`. **Solo lectura**: nunca se usa el refresh token ni se reescribe el archivo. Solo se acepta si `expires_at > now + 60s`. Home alterno: `KIMI_CODE_HOME`.
3. Cookie/token manual (Settings).
4. `KIMI_AUTH_TOKEN` (también minúscula `kimi_auth_token`).
5. Cookie `kimi-auth` de la app **Kimi Desktop** — `%APPDATA%\kimi-desktop\Cookies` en Windows ⚪ (en macOS: `~/Library/Application Support/kimi-desktop/Cookies` ✅ leído del código; el path Windows es inferencia y debe verificarse).
6. Cookies de navegador (dominios `www.kimi.com`, `kimi.com`).

**Device identity (cabeceras que la CLI oficial envía):** `User-Agent: CodexBar/<version>`, `X-Msh-Platform: kimi_code_cli`, `X-Msh-Version`, `X-Msh-Device-Name` (hostname, ASCII), `X-Msh-Device-Model` (`"{OS} {ver} {arch}"`), `X-Msh-Os-Version`, `X-Msh-Device-Id` (de `%USERPROFILE%\.kimi-code\device_id`; si falta, se crea con UUID v4 en minúsculas y permisos privados). Los valores no-ASCII se filtran a `0x20..0x7E`.

**Endpoints:**
| Uso | URL | Auth |
|---|---|---|
| Cuota Kimi Code (API key) | `GET https://api.kimi.com/coding/v1/usages` | `Authorization: Bearer <apiKey>` |
| Cuota web | `POST https://www.kimi.com/apiv2/kimi.gateway.billing.v1.BillingService/GetUsages` | `Authorization: Bearer <kimi-auth>` |
| Plan/membership (web) | `POST https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats` | Bearer `kimi-auth` + `Origin: https://www.kimi.com` + `Referer: https://www.kimi.com/code/console` |

Base API override: `KIMI_CODE_BASE_URL` (HTTPS, sin userinfo) — **desactiva** la reutilización del token de la CLI; solo se permite con API key explícita. Igual para `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST`.

**Respuesta** (`usages`): ventana principal (semanal) `limit/used/remaining/resetTime`, y `limits[]` con `window.duration`+`timeUnit` (p. ej. `300`+`TIME_UNIT_MINUTE` = rate limit de 5 h). Del response web, tomar `usages[].scope == "FEATURE_CODING"`. Memberships: Andante 1 024 req/sem, Moderato 2 048, Allegretto 7 168; todas 200 req/5 h.

### 3B. Moonshot / Kimi Open Platform (saldo)

- **API key:** `providers[].apiKey` o `MOONSHOT_API_KEY` / `MOONSHOT_KEY`. Bind por región: al cambiar de región **no** se envía la clave guardada al otro host.
- **Región:** `MOONSHOT_REGION` (`international` por defecto, `china`). Bases: `https://api.moonshot.ai` / `https://api.moonshot.cn`.
- **Endpoint:** `GET {base}/v1/users/me/balance` con `Authorization: Bearer <key>`, `Accept: application/json`.
- **Respuesta:** `available_balance`, `voucher_balance`, `cash_balance`. Solo saldo — **sin** ventanas de sesión/semana. Si `cash_balance < 0`, mostrar el déficit. Moneda USD (international) / CNY (china) sin conversión.
- Overrides internos de config: `CODEXBAR_MOONSHOT_API_KEY` + `CODEXBAR_MOONSHOT_API_KEY_REGION`.
- Detalle: `repo/Sources/CodexBarCore/Providers/Moonshot/` y `repo/docs/moonshot.md`.

---

## 4. MiniMax 🟡 (variable de entorno observada)

### 4.1 Credenciales
| Fuente | Variable / ruta |
|---|---|
| API token (Coding Plan) | `MINIMAX_CODING_API_KEY` **>** `MINIMAX_API_KEY` (el coding gana si ambos están); o config `apiKey` |
| Cookie manual | `MINIMAX_COOKIE` / `MINIMAX_COOKIE_HEADER` (header `Cookie:` crudo o "Copy as cURL"); o config `cookieHeader` |
| Cookies de navegador | dominios `platform.minimax.io`, `openplatform.minimax.io`, `minimax.io`, `platform.minimaxi.com`, `openplatform.minimaxi.com`, `minimaxi.com`; puede complementarse con tokens de **localStorage / sessionStorage / IndexedDB** de Chromium |

> **En esta máquina:** `MINIMAX_API_KEY` está **presente y no vacía** en el entorno de este shell (125 caracteres). No se leyó ni imprimió su valor y **no se validó** contra la API. El port debe tratar la mera presencia de la variable como "candidato", validando siempre contra el endpoint.

### 4.2 Hosts y región
- Global: `platform.minimax.io` (web) / `api.minimax.io` (API).
- China mainland: `platform.minimaxi.com` / `api.minimaxi.com`.
- Overrides: `MINIMAX_HOST`, `MINIMAX_CODING_PLAN_URL`, `MINIMAX_REMAINS_URL`, `MINIMAX_BILLING_HISTORY_URL`, `MINIMAX_REQUIRE_PROVIDER_ENDPOINT_OVERRIDES` (modo estricto: solo hosts `minimax.io`/`minimaxi.com`).

### 4.3 Endpoints
| Uso | Ruta |
|---|---|
| Página Coding Plan (HTML) | `{platform}/user-center/payment/coding-plan?cycle_type=3` |
| Remains Coding Plan / Token Plan | `{api}/v1/api/openplatform/coding_plan/remains`, `{api}/v1/token_plan/remains` — `Authorization: Bearer <token>` |
| Historial de facturación | `{platform}/account/amount?page=1&limit=100&aggregate=false` — Bearer |

**Selección (Auto):** API token primero; si es rechazado o el host global devuelve 404 → reintento en host China → luego fallback a la ruta web/cookie. Con cookie/sesión se hace scrape de la página Coding Plan y, si falla, el endpoint `remains`; el historial de facturación (chart 30 días, top model/method) es **best-effort**: si falla, se conserva la cuota.

**Diagnóstico:** `codexbar diagnose --provider minimax` redacta tokens (`sk-cp-*`, `sk-api-*`), cookies, emails, org IDs — replicar esa redacción en logs.

Detalle: `repo/Sources/CodexBarCore/Providers/MiniMax/`, `repo/docs/minimax.md`.

---

## 5. z.ai (GLM) 🟡

### 5.1 Token (orden de fallback exacto, `ZaiSettingsReader`)
1. `providers[].apiKey` del config.
2. `Z_AI_API_KEY` (región seleccionada).
3. **Solo China:** `BIGMODEL_API_KEY`, `ZHIPU_API_KEY`, `ZHIPUAI_API_KEY`, `GLM_API_KEY`.
4. **Solo China, primer archivo legible de una línea:**
   - `%USERPROFILE%\.coding-relay\glm-api-key` (macOS/Linux `~/.coding-relay/glm-api-key`)
   - `%USERPROFILE%\.config\bigmodel\api_key`
   - `%USERPROFILE%\.config\zhipu\api_key`

Los alias BigModel y los archivos relay **nunca** se usan para la ruta global `api.z.ai`.

### 5.2 Región y endpoints
- Global: `https://api.z.ai`; China: `https://open.bigmodel.cn`.
- Cuota: `GET {base}/api/monitor/usage/quota/limit` (team añade `?type=2`).
- Uso de modelos: `GET {base}/api/monitor/usage/model-usage?startTime=YYYY-MM-DD HH:MM:SS&endTime=…` (team añade `&type=3`).
- Saldo CN (pay-as-you-go, best-effort, 5 s): `GET https://www.bigmodel.cn/api/biz/account/query-customer-account-report`.
- Headers: `Authorization: Bearer <token>`, `Accept: application/json`; equipo añade `Bigmodel-Organization: <orgId>`, `Bigmodel-Project: <projectId>`.

### 5.3 Overrides y discrepancia de nombres (⚠️ importante para el port)
El Swift lee `Z_AI_API_HOST`, `Z_AI_QUOTA_URL`, `Z_AI_BALANCE_URL` (`ZaiSettingsReader`), pero el plugin JS (`Resources/Plugins/zai.js`) recibe del descriptor `Z_AI_REGION`, `Z_AI_USAGE_SCOPE`, `Z_AI_QUOTA_ENDPOINT`, `Z_AI_MODEL_USAGE_ENDPOINT`, `Z_AI_BALANCE_ENDPOINT`, `Z_AI_ORGANIZATION`, `Z_AI_PROJECT`. **El port debe soportar ambos conjuntos de nombres** (los `*_URL`/`Z_AI_API_HOST` como entrada de usuario, los `*_ENDPOINT`/`Z_AI_REGION` como forma que ve la capa de fetch). Validación: HTTPS obligatorio; host `api.z.ai` no puede sobreescribir una selección CN y viceversa (error `endpointRegionMismatch`).

### 5.4 Parseo
- Respuesta debe cumplir `success == true && code == 200`, `data.limits[]`.
- Tipos: `TOKENS_LIMIT` y `CREDIT_LIMIT` → ventanas del Coding Plan; `TIME_LIMIT` → carril MCP separado.
- Por límite: `unit`+`number` → minutos (multiplicadores `{1:1440(day), 3:60(hour), 5:1(minute), 6:10080(week)}`), `percentage` (entero, requerido). Si hay `usage>0` y `currentValue`/`remaining`, el porcentaje se recalcula de los conteos y se clampa 0–100.
- Reset: `nextResetTime` (epoch ms). Un reset de plan 5 h que caiga a >5 h + 60 s en el futuro se **omite** (nunca se adivina corrección de zona horaria).
- Ordenamiento: por duración; con múltiples límites el primero es `primary` y el último `secondary`.
- Plan: `data.planName` / `plan` / `plan_type` / `packageName` / `level`.
- Fila "Quota rate" (solo planes de crédito): peak Lun–Vie 06:00–10:00 UTC; 1× peak / 0.5× off-peak.

Detalle: `repo/Sources/CodexBarCore/Resources/Plugins/zai.js`, `repo/Sources/CodexBarCore/Providers/Zai/`, `repo/docs/zai.md`.

---

## 6. OpenRouter 🟡

### 6.1 Credenciales
- `OPENROUTER_API_KEY` (bearer) o config `apiKey`.
- `OPENROUTER_MANAGEMENT_API_KEY` (opcional; solo para la Activity exacta de 30 días).
- Windows: OpenRouter no tiene archivo de config propio; la fuente es variable de entorno o el config del port. (Posible fuente extra **no verificada**: entradas `openrouter` en `%USERPROFILE%\.local\share\opencode\auth.json` — en esta máquina ese archivo solo tenía `opencode-go`.)

### 6.2 Config y overrides
`OPENROUTER_API_URL` (default `https://openrouter.ai/api/v1`), `OPENROUTER_HTTP_REFERER`, `OPENROUTER_X_TITLE` (default `CodexBar`). `OPENROUTER_API_URL` debe ser HTTPS.

### 6.3 Endpoints
| Uso | URL | Notas |
|---|---|---|
| Créditos | `GET {base}/credits` | obligatorio; `data.total_credits`, `data.total_usage` |
| Cuota de la key | `GET {base}/key` | **opcional**, deadline 1 s; degradación suave |
| Activity (spend) | `GET https://openrouter.ai/api/v1/activity` (host **fijo**, nunca sigue el override) y `?date=YYYY-MM-DD` | requiere management key |

**Balance** = `max(0, total_credits - total_usage)`.
**Degradación:** si `/key` tarda/falla, se muestran los créditos y la fila "API key limit: Unavailable right now" con diagnóstico (timeout/HTTP/JSON). El estado del mensaje de management ausente es "Management API key not configured" o "Management API key required" (403).

### 6.4 Parseo de `/key`
Campos: `limit`, `limit_remaining`, `usage`, `usage_daily`, `usage_weekly`, `usage_monthly`, `limit_reset` (string), `rate_limit{requests(int),interval(string)}`. Todos `f64` finitos u `null`.
**Porcentaje primario:** 1) `limit - clamp(limit_remaining, 0, limit)` si `limit_remaining` está presente; 2) `usage_{daily|weekly|monthly}` según `limit_reset`; 3) `usage` acumulado. `limit>0` y `used>=0` requeridos para publicar el meter.

### 6.5 Activity (30 días UTC)
Validaciones estrictas (replicar): fecha `YYYY-MM-DD[ HH:MM:SS]`, debe ser día UTC **completado**, `≥ cutoff`; modelo ≤64 chars (`model_permaslug ?? model`); `prompt_tokens`/`completion_tokens`/`reasoning_tokens`/`requests` enteros seguros ≥0; `reasoning_tokens ≤ completion_tokens`; `cost = usage + byok_usage_inference`. Deduplicar por `(date, model, endpoint_id, provider_name, workspace_id)` con firma de tokens/costos (conflicto duplicado = error). Límites: >20 000 filas brutas o >10 000 distintas → error. `historyLabel = "Last 30 days (UTC)"`.

Detalle: `repo/Sources/CodexBarCore/Resources/Plugins/openrouter.js`, `.../Providers/OpenRouter/`, `repo/docs/openrouter.md`.

---

## 7. DeepSeek 🟡

### 7.1 Credenciales
- API key: `DEEPSEEK_API_KEY` o `DEEPSEEK_KEY` (bearer), o config `apiKey`.
- Sesión de plataforma (para uso detallado): `DEEPSEEK_PLATFORM_TOKEN` / `DEEPSEEK_USER_TOKEN`, o `userToken` del **localStorage de Chrome** en el origen `https://platform.deepseek.com`.
  - Windows (navegador): `%LOCALAPPDATA%\Google\Chrome\User Data\<Profile>\Local Storage\leveldb` (LevelDB) ⚪ no verificado en vivo.
- Scoping anti-fuga: `CODEXBAR_DEEPSEEK_PROFILE_ID`, `CODEXBAR_DEEPSEEK_PROFILE_SCOPE`; el scope se deriva como `SHA256("com.steipete.codexbar.deepseek-profile-scope.v1\0<accountID>\0<apiKey>")`. Un token sin scope nunca se combina con la API key; los tokens importados viven **en memoria**.

### 7.2 Endpoints
| Uso | URL | Auth |
|---|---|---|
| Saldo (API key) | `GET https://api.deepseek.com/user/balance` | `Authorization: Bearer <apiKey>`, `Accept: application/json` |
| Saldo (sesión plataforma) | `GET https://platform.deepseek.com/api/v0/users/get_user_summary` | `Bearer <userToken>` |
| Uso cantidad | `GET https://platform.deepseek.com/api/v0/usage/amount?month=&year=` | `Bearer <userToken>` |
| Uso costo | `GET https://platform.deepseek.com/api/v0/usage/cost?month=&year=` | `Bearer <userToken>` |

Saldo: `is_available` + `balance_infos[]` con `{currency, total_balance, granted_balance, topped_up_balance}`. Preferir **USD** si hay varias monedas. Si balance 0 → "agregar créditos"; si balance >0 y `is_available=false` → "Balance unavailable for API calls". Errores de sesión: códigos `40002`/`40003` (top-level o anidados) = sesión vencida. `amount` y `cost` corren en paralelo con deadline de 5 s; un fallo de detalle **no** borra el saldo. **No hay ventana sesión/semana.**

Detalle: `repo/Sources/CodexBarCore/Providers/DeepSeek/` (incl. `DeepSeekPlatformTokenImporter.swift`), `repo/docs/deepseek.md`.

---

## 8. Groq 🟡

Dos fuentes; el console web es la preferida y **no** es API key (es sesión de navegador). Es el único de los 9 cuyo camino principal es cookie-based.

### 8.1 Fuentes / modos (`web` | `api` | `auto`)
1. **Console web (preferido):** cookies de `groq.com` / `console.groq.com`; la API de actividad devuelve spend/tokens/requests diarios.
2. **Prometheus (fallback, solo Enterprise):** requiere `GROQ_API_KEY` (o config). Claves estándar reciben **HTTP 404** aquí → sin datos.

### 8.2 Console web
- Cookies: `stytch_session` (opaco, ~30 días, preferido) → si no, `stytch_session_jwt` (JWT corto, ~5 min). Extraer de un header `Cookie:` manual también se admite.
- **Refresh de JWT** (cada fetch): `POST https://api.stytchb2b.groq.com/sdk/v1/b2b/sessions/authenticate` con `Authorization: Basic base64("<publicToken>:<sessionToken>")`, headers `X-SDK-Client`, `X-SDK-Parent-Host`, `Origin: https://console.groq.com`. El public token de Stytch va embebido (publishable); override con `GROQ_STYTCH_PUBLIC_TOKEN` / `GROQ_STYTCH_URL`.
- **Actividad:** `GET https://api.groq.com/platform/v1/organizations/{orgId}/activity?start_date=<unix>&end_date=<unix>` con `Authorization: Bearer <sessionJWT>`. `orgId` se lee del claim del JWT `https://groq.com/organization` (sin verificar firma; solo routing).
- Filas por modelo/día: `cost`, `n_context_tokens_total`, `n_non_cached_context_tokens_total` (cached = context − non-cached), `n_generated_tokens_total`, `num_requests`. Se agregan a buckets diarios → chart compartido.

### 8.3 Prometheus (Enterprise)
`GET https://api.groq.com/v1/metrics/prometheus/api/v1/query?query=…` con las series `sum(model_project_id_status_code:requests:rate5m)`, `model_project_id:tokens_in:rate5m`, `model_project_id:tokens_out:rate5m`, `model_project_id:prompt_cache_hits:rate5m`. Base override `GROQ_API_URL` (default `https://api.groq.com/v1`).

### 8.4 Overrides de prueba
`GROQ_SESSION_TOKEN` (opaco, ejercita el refresh), `GROQ_SESSION_JWT` (JWT directo, sin refresh).

Detalle: `repo/Sources/CodexBarCore/Providers/Groq/`, `repo/docs/groq.md`.

---

## 9. ElevenLabs 🟡

- **API key:** `ELEVENLABS_API_KEY` o `XI_API_KEY`, o config `apiKey`. Base override `ELEVENLABS_API_URL` (default `https://api.elevenlabs.io`, HTTPS).
- **Endpoint:** `GET https://api.elevenlabs.io/v1/user/subscription` — header **`xi-api-key: <key>`** (⚠️ **no** `Authorization: Bearer`), `Accept: application/json`. Timeout 15 s.
- **Campos:** `character_count`, `character_limit`, `voice_slots_used`, `voice_limit`, `professional_voice_slots_used`, `professional_voice_limit`, `current_overage{amount,currency}`, `tier`, `status`, `next_character_count_reset_unix` (epoch s → reset).
- **Mapeo:** primario = `character_count/character_limit` (clamp 0–100), descripción `"<used> / <limit> credits"`; ventanas extra `voice-slots` y `professional-voices` (solo si `limit>0`); identity = tier capitalizado + `· status` si no es `active`.
- **Errores (leer `detail.code`, luego legacy `detail.status`):** `invalid_api_key` → clave inválida; `missing_permissions`/`insufficient_permissions` → falta `user_read`; 401 sin código → auth falló; 403 sin código → acceso denegado (permissions/IP allowlist).

Detalle: `repo/Sources/CodexBarCore/Providers/ElevenLabs/`, `repo/docs/elevenlabs.md`.

---

## 10. xAI (plataforma developer / Grok) 🟡

⚠️ Distinto del proveedor **Grok** (suscripción consumer, vía Grok CLI / grok.com, cubierto por `Providers/Grok/`, **fuera de alcance aquí**). No comparten credenciales ni saldos.

- **Credenciales:** `XAI_MANAGEMENT_API_KEY` + `XAI_TEAM_ID`, o config `apiKey` + `workspaceID`. Claves de inferencia **no** funcionan en la Management API. Validación del team ID: sin `/`, distinto de `.` y `..`.
- **Endpoints** (base `https://management-api.x.ai`, `Authorization: Bearer <key>`):
  - Saldo: `GET /v1/billing/teams/{team_id}/prepaid/balance`.
  - Gasto diario (best-effort): `POST /v1/billing/teams/{team_id}/usage` con body `{analyticsRequest:{timeRange:{startTime,endTime,timezone:"Etc/GMT"}, timeUnit:"TIME_UNIT_DAY", values:[{name:"usd",aggregation:"AGGREGATION_SUM"}], groupBy:[], filters:[]}}`, ventana últimos 30 días UTC.
- **Saldo:** `total.val` es string en **céntimos invertidos** (`"-1000"` = +$10). Balance = `-Number(val)/100`. Un total no parseable = **error**, nunca $0.00. El balance mostrado es el **posted** del ledger (puede ser mayor que el vivo a mitad de ciclo).
- **Gasto:** `timeSeries[].dataPoints[]` → `(timestamp, values[0])` sumados por día UTC → puntos de chart. `limitReached === true` → partial → `dataConfidence:"estimated"` y etiqueta "Last 30 days (partial)".
- **Errores:** 401/403 → clave rechazada; 404 → team ID equivocado o de otro equipo; 429 → rate limit. Un fallo de historial no suprime el saldo.

Detalle: `repo/Sources/CodexBarCore/Resources/Plugins/xai.js`, `.../Providers/XAI/`, `repo/docs/xai.md`.

---

## 11. Resumen de credenciales por proveedor

| Proveedor | Env var(s) principal(es) | Archivo local (Windows) | Tipo de auth |
|---|---|---|---|
| OpenRouter | `OPENROUTER_API_KEY`, `OPENROUTER_MANAGEMENT_API_KEY` | — | Bearer |
| DeepSeek | `DEEPSEEK_API_KEY`/`DEEPSEEK_KEY`; `DEEPSEEK_PLATFORM_TOKEN`/`DEEPSEEK_USER_TOKEN` | Chrome localStorage (`userToken`) ⚪ | Bearer |
| Groq | `GROQ_API_KEY` (Enterprise); `GROQ_SESSION_TOKEN`/`GROQ_SESSION_JWT` | cookies navegador (`stytch_session`) | Cookie / Bearer JWT |
| z.ai (GLM) | `Z_AI_API_KEY`; CN: `BIGMODEL_API_KEY`,`ZHIPU_API_KEY`,`ZHIPUAI_API_KEY`,`GLM_API_KEY` | CN: `.coding-relay\glm-api-key`, `.config\bigmodel\api_key`, `.config\zhipu\api_key` | Bearer |
| MiniMax | `MINIMAX_CODING_API_KEY` > `MINIMAX_API_KEY`; `MINIMAX_COOKIE(_HEADER)` | cookies + storage Chromium | Bearer / Cookie |
| Kimi Code | `KIMI_CODE_API_KEY`; `KIMI_AUTH_TOKEN` | `.kimi-code\credentials\kimi-code.json`, `.kimi-code\device_id` | Bearer / Cookie |
| Moonshot | `MOONSHOT_API_KEY`/`MOONSHOT_KEY` | — | Bearer |
| ElevenLabs | `ELEVENLABS_API_KEY`/`XI_API_KEY` | — | `xi-api-key` header |
| xAI | `XAI_MANAGEMENT_API_KEY`, `XAI_TEAM_ID` | — | Bearer |
| OpenCode Go | `OPENCODE_API_KEY` | `.local\share\opencode\opencode.db` ✅, `.local\share\opencode\auth.json` ✅ | Bearer / local |

---

## 12. Matriz de verificación (estado en esta máquina, 2026-09-10)

| Ítem | Estado | Evidencia / razón |
|---|---|---|
| Ruta y schema de OpenCode Go local (`opencode.db`) | ✅ | Archivo existe (516 096 B); tablas `message`,`part` confirmadas; la **consulta exacta del lector devolvió 4 filas** `opencode-go` |
| `auth.json` con entrada `opencode-go.key` | ✅ | JSON parseado; claves `key`,`type` presentes (valores no leídos) |
| Navegadores presentes para import de cookies | ✅ | Chrome/Edge/Firefox dirs existen |
| `MINIMAX_API_KEY` presente en el entorno del shell | ✅ (presencia) | Variable seteada, 125 chars; valor **no** leído; API **no** validada |
| Lógica de endpoints y parseo de los 9 | 🟡 | Extraída del Swift/JS del repo; sin credenciales reales no se pudo ejecutar |
| OpenRouter / DeepSeek / Groq / z.ai / Moonshot / ElevenLabs / xAI keys | ⚪ | Ninguna variable de entorno ni archivo de config presente; nunca se hicieron llamadas reales |
| Kimi Code CLI (`kimi-code.json`) | ⚪ | `%USERPROFILE%\.kimi-code\` no existe (archivo y `device_id` ausentes) |
| Path Windows de la app Kimi Desktop / Chrome localStorage DeepSeek / Groq | ⚪ | Inferidos por convención; verificar en implementación |
| `codexbar` config propio (`~/.config/codexbar/config.json`) | ⚪ | No existe en esta máquina |

**No se modificó ningún archivo de `repo/`** y **no se hizo login ni escritura de credenciales**. Todos los comandos fueron de lectura; los secretos se trataron como opacos (solo nombres, longitudes y presencia).
