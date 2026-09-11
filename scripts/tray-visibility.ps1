# tray-visibility.ps1 — prove whether the CodexBar tray icons are visible and clickable.
#
# Windows 11 hides brand-new tray icons in the overflow flyout ("hidden icon
# menu"), so a screenshot of the notification area alone proves nothing. This
# script:
#   1. finds the tray chevron on the PRIMARY taskbar with UI Automation
#      (`SystemTrayIcon` / "Show hidden icons"), locale-independent-ish,
#   2. clicks it with a real synthesized mouse click,
#   3. enumerates the overflow flyout and matches the icons whose UIA Name is a
#      CodexBar tooltip ("Codex — 14% used", ...),
#   4. optionally clicks one of them (real mouse click) and screenshots the
#      result — that is the "the icon responds" evidence,
#   5. closes the flyout again.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File tray-visibility.ps1 `
#       -OutDir C:\path\evidence [-ClickName "Codex"] [-HoldSeconds 2]

param(
  [Parameter(Mandatory = $true)][string]$OutDir,
  [string]$ClickName = "",
  [int]$HoldSeconds = 2,
  [switch]$RightClick
)

$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class TrayMouse {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, IntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
  public const uint LEFTDOWN = 0x0002, LEFTUP = 0x0004, RIGHTDOWN = 0x0008, RIGHTUP = 0x0010;
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(150);
    mouse_event(LEFTDOWN, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(70);
    mouse_event(LEFTUP, 0, 0, 0, IntPtr.Zero);
  }
  public static void RightClick(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(150);
    mouse_event(RIGHTDOWN, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(70);
    mouse_event(RIGHTUP, 0, 0, 0, IntPtr.Zero);
  }
}
"@

function Save-ScreenRect([string]$path, [System.Drawing.Rectangle]$rect) {
  if ($rect.Width -le 0 -or $rect.Height -le 0) { return $false }
  $bmp = New-Object System.Drawing.Bitmap $rect.Width, $rect.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $rect.Width, $rect.Height))
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  return $true
}

function Save-FullScreen([string]$path) {
  return (Save-ScreenRect $path ([System.Windows.Forms.SystemInformation]::VirtualScreen))
}

$root = [System.Windows.Automation.AutomationElement]::RootElement
$desc = [System.Windows.Automation.TreeScope]::Descendants
$children = [System.Windows.Automation.TreeScope]::Children
$true_cond = [System.Windows.Automation.Condition]::TrueCondition

function Get-ElementRect($el) {
  $r = $el.Current.BoundingRectangle
  return (New-Object System.Drawing.Rectangle ([int]$r.X), ([int]$r.Y), ([int]$r.Width), ([int]$r.Height))
}

# ---- 1. the primary taskbar and its chevron ---------------------------------
$cond = New-Object System.Windows.Automation.PropertyCondition(
  [System.Windows.Automation.AutomationElement]::ClassNameProperty, "Shell_TrayWnd")
$taskbar = $root.FindFirst($children, $cond)
if (-not $taskbar) { Write-Output "FAIL: Shell_TrayWnd not found"; exit 2 }

$chevronNames = @("Mostrar iconos ocultos", "Show hidden icons", "Notification Chevron", "Notification overflow")
$chevron = $null
foreach ($e in $taskbar.FindAll($desc, $true_cond)) {
  $name = ""; try { $name = $e.Current.Name } catch {}
  $aid = ""; try { $aid = $e.Current.AutomationId } catch {}
  if ($chevronNames -contains $name -or $aid -eq "NotifyChevron") {
    $chevron = $e
    Write-Output "chevron found: name='$name' id='$aid'"
    break
  }
}

if (-not $chevron) { Write-Output "FAIL: notification chevron not found in the taskbar"; exit 3 }

$cr = Get-ElementRect $chevron
$cx = $cr.Left + [int]($cr.Width / 2)
$cy = $cr.Top + [int]($cr.Height / 2)
Write-Output "clicking chevron at $cx,$cy"
[TrayMouse]::Click($cx, $cy)
Start-Sleep -Milliseconds 1500

# ---- 2. find the overflow flyout -------------------------------------------
$flyoutClasses = @("TopLevelWindowForOverflowXamlIsland", "NotifyIconOverflowWindow", "Xaml_WindowedPopupClass")
$flyout = $null
Write-Output "top-level windows after the click:"
foreach ($w in $root.FindAll($children, $true_cond)) {
  $cls = ""; try { $cls = $w.Current.ClassName } catch {}
  $nm = ""; try { $nm = $w.Current.Name } catch {}
  if ($nm -or $cls) { Write-Output "  class='$cls' name='$nm'" }
  if ($flyoutClasses -contains $cls -and -not $flyout) { $flyout = $w }
}

$report = @()
if ($flyout) {
  $fr = Get-ElementRect $flyout
  $shot = Join-Path $OutDir "tray-flyout.png"
  $null = Save-ScreenRect $shot $fr
  Write-Output "flyout class='$($flyout.Current.ClassName)' @$($fr.X),$($fr.Y) $($fr.Width)x$($fr.Height) -> $shot"

  $items = $flyout.FindAll($desc, $true_cond)
  Write-Output "flyout descendant count: $($items.Count)"
  $hits = @()
  foreach ($e in $items) {
    $name = ""; try { $name = $e.Current.Name } catch {}
    if (-not $name) { continue }
    $ct = ""; try { $ct = $e.Current.ControlType.ProgrammaticName } catch {}
    $aid = ""; try { $aid = $e.Current.AutomationId } catch {}
    $r = Get-ElementRect $e
    $report += [pscustomobject]@{ name = $name; type = $ct; id = $aid; x = $r.X; y = $r.Y; w = $r.Width; h = $r.Height }
    if ($r.Width -gt 4 -and $r.Height -gt 4) {
      Write-Output ("  item: {0,-8} {1,-42} @{2},{3} {4}x{5}" -f $ct, $name, $r.X, $r.Y, $r.Width, $r.Height)
    }
    if ($name -match "Codex|Claude|Cursor|OpenRouter|Copilot|Gemini") {
      $hits += [pscustomobject]@{ element = $e; name = $name; x = $r.X; y = $r.Y; w = $r.Width; h = $r.Height }
    }
  }

  New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
  $report | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $OutDir "tray-flyout-uia.json") -Encoding UTF8
  Write-Output "CODEBAR_ICON_HITS=$($hits.Count)"
  foreach ($h in $hits) { Write-Output "  hit: $($h.name)" }

  # ---- 3. click a CodexBar icon (real mouse click) --------------------------
  if ($ClickName -ne "" -and $hits.Count -gt 0) {
    $target = $hits | Where-Object { $_.name -match $ClickName } | Select-Object -First 1
    if (-not $target) { $target = $hits[0] }
    $tx = $target.x + [int]($target.w / 2)
    $ty = $target.y + [int]($target.h / 2)
    if ($RightClick) {
      Write-Output "right-clicking tray icon '$($target.name)' at $tx,$ty"
      [TrayMouse]::RightClick($tx, $ty)
      Start-Sleep -Seconds $HoldSeconds
      $shot2 = Join-Path $OutDir "tray-menu.png"
    } else {
      Write-Output "clicking tray icon '$($target.name)' at $tx,$ty"
      [TrayMouse]::Click($tx, $ty)
      Start-Sleep -Seconds $HoldSeconds
      $shot2 = Join-Path $OutDir "tray-click-result.png"
    }
    $null = Save-FullScreen $shot2
    if ($RightClick) { [TrayMouse]::Click($tx, $ty) }   # close the menu again
    $null = Save-FullScreen $shot2
    Write-Output "post-click screenshot -> $shot2"
  }
} else {
  Write-Output "FAIL: no overflow flyout window appeared after clicking the chevron"
  $shot2 = Join-Path $OutDir "tray-after-chevron.png"
  $null = Save-FullScreen $shot2
  Write-Output "screenshot -> $shot2"
}

# ---- 4. close the flyout again ---------------------------------------------
[TrayMouse]::Click($cx, $cy)
Write-Output "done"
