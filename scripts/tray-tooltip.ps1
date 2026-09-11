# tray-tooltip.ps1 — capture the REAL tray tooltip of a CodexBar icon.
#
# The overflow flyout is opened (chevron click, like tray-visibility.ps1), then a
# CodexBar icon is *hovered* (no click) so the shell shows its tooltip — which is
# where the per-provider state and the live/mock source indicator live — and the
# tooltip window (class `tooltips_class32`) is screenshotted.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File tray-tooltip.ps1 -OutDir C:\...\evidence

param(
  [Parameter(Mandatory = $true)][string]$OutDir,
  [int]$HoldMs = 2500
)

$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class TTHover {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@

$root = [System.Windows.Automation.AutomationElement]::RootElement
$children = [System.Windows.Automation.TreeScope]::Children
$desc = [System.Windows.Automation.TreeScope]::Descendants
$true_cond = [System.Windows.Automation.Condition]::TrueCondition

function Get-Rect($el) {
  $b = $el.Current.BoundingRectangle
  return (New-Object System.Drawing.Rectangle ([int]$b.X), ([int]$b.Y), ([int]$b.Width), ([int]$b.Height))
}
function Save-Rect([string]$path, [System.Drawing.Rectangle]$rect) {
  if ($rect.Width -le 0 -or $rect.Height -le 0) { return $false }
  $bmp = New-Object System.Drawing.Bitmap $rect.Width, $rect.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $rect.Width, $rect.Height))
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  return $true
}
function Find-ByClass([string]$class) {
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ClassNameProperty, $class)
  return $root.FindFirst($children, $cond)
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

# ---- 1. open the overflow flyout if it is not already up --------------------
$flyout = Find-ByClass "TopLevelWindowForOverflowXamlIsland"
if (-not $flyout) {
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ClassNameProperty, "Shell_TrayWnd")
  $taskbar = $root.FindFirst($children, $cond)
  if (-not $taskbar) { Write-Output "FAIL: Shell_TrayWnd not found"; exit 2 }
  $chevron = $null
  foreach ($e in $taskbar.FindAll($desc, $true_cond)) {
    $n = ""; try { $n = $e.Current.Name } catch {}
    $a = ""; try { $a = $e.Current.AutomationId } catch {}
    if ($n -match "iconos ocultos|hidden icons" -or $a -eq "NotifyChevron" -or $n -eq "SystemTrayIcon") {
      $chevron = $e; break
    }
  }
  if (-not $chevron) { Write-Output "FAIL: no chevron/notification-area element"; exit 3 }
  $cr = Get-Rect $chevron
  $cx = $cr.Left + [int]($cr.Width / 2); $cy = $cr.Top + [int]($cr.Height / 2)
  Write-Output "clicking chevron at $cx,$cy"
  [TTHover]::SetCursorPos($cx, $cy); Start-Sleep -Milliseconds 200
  Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e); public static void Left() { mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(80); mouse_event(0x0004,0,0,0,IntPtr.Zero); }' -Name M2 -Namespace TTHoverNs | Out-Null
  [TTHoverNs.M2]::Left()
  Start-Sleep -Milliseconds 1500
  $flyout = Find-ByClass "TopLevelWindowForOverflowXamlIsland"
}
if (-not $flyout) { Write-Output "FAIL: overflow flyout did not appear"; exit 4 }
Write-Output "flyout @ $((Get-Rect $flyout).X),$((Get-Rect $flyout).Y)"

# ---- 2. find a CodexBar icon by its tooltip name ----------------------------
$pattern = "Codex|Claude|Cursor|OpenRouter|Copilot|Gemini|DeepSeek|Groq|z\.ai|MiniMax|Kimi|ElevenLabs|xAI|OpenCode|no data|sample data"
$icon = $null
foreach ($e in $flyout.FindAll($desc, $true_cond)) {
  $n = ""; try { $n = $e.Current.Name } catch {}
  if (-not $n) { continue }
  $r = Get-Rect $e
  Write-Output ("  icon: '{0}' @{1},{2} {3}x{4}" -f $n, $r.X, $r.Y, $r.Width, $r.Height)
  if (-not $icon -and $n -match $pattern) { $icon = $e }
}
if (-not $icon) { Write-Output "CODEBAR_ICON_HITS=0"; exit 5 }
$ir = Get-Rect $icon
Write-Output "CODEBAR_ICON_HITS>=1; hovering '$($icon.Current.Name)'"

# ---- 3. hover until the tooltip appears, then capture it --------------------
for ($try = 0; $try -lt 6 -and -not $tip; $try++) {
  [TTHover]::SetCursorPos(($ir.Left + [int]($ir.Width / 2)), ($ir.Top + [int]($ir.Height / 2)))
  Start-Sleep -Milliseconds 400
  [TTHover]::SetCursorPos(($ir.Left + [int]($ir.Width / 2) + 1), ($ir.Top + [int]($ir.Height / 2)))
  Start-Sleep -Milliseconds 900
  $tip = Find-ByClass "tooltips_class32"
}
if (-not $tip) { Write-Output "FAIL: no tooltip window after hovering"; exit 6 }

$tr = Get-Rect $tip
$tipName = ""; try { $tipName = $tip.Current.Name } catch {}
Write-Output "tooltip window @ $($tr.X),$($tr.Y) $($tr.Width)x$($tr.Height) name='$tipName'"
$pad = New-Object System.Drawing.Rectangle (($tr.Left - 4), ($tr.Top - 4), ($tr.Width + 8), ($tr.Height + 8))
if (Save-Rect (Join-Path $OutDir "tray-tooltip.png") $pad) {
  Write-Output "wrote $(Join-Path $OutDir 'tray-tooltip.png')"
}
# The full hover region (icon + tooltip) is the readable artifact.
$left = [Math]::Min($ir.Left, $tr.Left); $top = [Math]::Min($ir.Top, $tr.Top)
$right = [Math]::Max($ir.Right, $tr.Right); $bottom = [Math]::Max($ir.Bottom, $tr.Bottom)
$whole = New-Object System.Drawing.Rectangle (($left - 6), ($top - 6), ($right - $left + 12), ($bottom - $top + 12))
if (Save-Rect (Join-Path $OutDir "tray-tooltip-icon.png") $whole) {
  Write-Output "wrote $(Join-Path $OutDir 'tray-tooltip-icon.png')"
}
Start-Sleep -Milliseconds $HoldMs

# ---- 4. put the shell back the way we found it ------------------------------
[TTHover]::SetCursorPos(1200, 200)
Start-Sleep -Milliseconds 300
Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e); public static void Left() { mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(80); mouse_event(0x0004,0,0,0,IntPtr.Zero); }' -Name M3 -Namespace TTHoverNs | Out-Null
[TTHoverNs.M3]::Left()
Write-Output "done"
