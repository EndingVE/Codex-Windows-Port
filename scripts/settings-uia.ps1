# settings-uia.ps1 — drive the CodexBar settings window through UI Automation.
#
# Reads the accessibility tree of the settings webview (proof that the page
# really rendered the persisted settings), then optionally toggles checkboxes
# and presses Save so the whole
#   checkbox → invoke("set_settings") → config.json → tray rebuild
# path is exercised without a human in the loop.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File settings-uia.ps1 -OutDir C:\...\evidence `
#       -Toggle "Merge into one icon" -Toggle "Start CodexBar when I sign in" -Press Save

param(
  [Parameter(Mandatory = $true)][string]$OutDir,
  [string[]]$Toggle = @(),
  [string[]]$Press = @(),
  [switch]$DumpOnly
)

$ErrorActionPreference = "Continue"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class SUIA {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
  public const uint LEFTDOWN = 0x0002, LEFTUP = 0x0004;
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(150);
    mouse_event(LEFTDOWN, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(70);
    mouse_event(LEFTUP, 0, 0, 0, IntPtr.Zero);
  }
}
"@

$root = [System.Windows.Automation.AutomationElement]::RootElement
$children = [System.Windows.Automation.TreeScope]::Children
$desc = [System.Windows.Automation.TreeScope]::Descendants

function Find-Window([string]$title) {
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, $title)
  return $root.FindFirst($children, $cond)
}

function Rect($el) {
  try {
    $r = $el.Current.BoundingRectangle
    $x = [double]$r.X; $y = [double]$r.Y; $w = [double]$r.Width; $h = [double]$r.Height
    if ([double]::IsNaN($x) -or [double]::IsInfinity($x)) { $x = -100000 }
    if ([double]::IsNaN($y) -or [double]::IsInfinity($y)) { $y = -100000 }
    if ([double]::IsNaN($w) -or [double]::IsInfinity($w)) { $w = 0 }
    if ([double]::IsNaN($h) -or [double]::IsInfinity($h)) { $h = 0 }
    $x = [Math]::Max(-100000, [Math]::Min(100000, $x))
    $y = [Math]::Max(-100000, [Math]::Min(100000, $y))
    $w = [Math]::Max(0, [Math]::Min(100000, $w))
    $h = [Math]::Max(0, [Math]::Min(100000, $h))
    return (New-Object System.Drawing.Rectangle ([int]$x), ([int]$y), ([int]$w), ([int]$h))
  } catch {
    return (New-Object System.Drawing.Rectangle 0, 0, 0, 0)
  }
}

function Click-Element($el) {
  $r = Rect $el
  if ($r.Width -le 0) { return $false }
  [SUIA]::Click(($r.Left + [int]($r.Width / 2)), ($r.Top + [int]($r.Height / 2)))
  return $true
}

$win = Find-Window "CodexBar Settings"
if (-not $win) {
  for ($i = 0; $i -lt 20 -and -not $win; $i++) { Start-Sleep -Milliseconds 500; $win = Find-Window "CodexBar Settings" }
}
if (-not $win) { Write-Output "FAIL: 'CodexBar Settings' window not found"; exit 2 }

[void][SUIA]::SetForegroundWindow($win.Current.NativeWindowHandle)
Start-Sleep -Milliseconds 700

# ---- 1. dump the accessibility tree ----------------------------------------
$entries = @()
foreach ($e in $win.FindAll($desc, [System.Windows.Automation.Condition]::TrueCondition)) {
  try {
    $name = $e.Current.Name
    $ct = $e.Current.ControlType.ProgrammaticName
  } catch { continue }
  if (-not $name -and $ct -notmatch "Text|Edit") { continue }
  $r = Rect $e
  $entries += [pscustomobject]@{
    controlType = $ct; name = $name; id = $e.Current.AutomationId
    x = $r.X; y = $r.Y; w = $r.Width; h = $r.Height; enabled = $e.Current.IsEnabled
  }
}
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$entries | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $OutDir "settings-uia-tree.json") -Encoding UTF8
Write-Output "UIA elements in the settings window: $($entries.Count)"
foreach ($e in $entries) {
  if ($e.controlType -match "CheckBox|Button|Edit|Text" -and $e.name) {
    Write-Output ("  {0,-16} {1}" -f $e.controlType.Replace("ControlType.",""), $e.name)
  }
}

if ($DumpOnly) { Write-Output "dump only"; exit 0 }

function Invoke-Named([string]$name, [string]$kind) {
  $hits = $entries | Where-Object { $_.name -eq $name -and $_.controlType -match $kind }
  if (-not $hits) { return "not found: $name ($kind)" }
  $el = $win.FindAll($desc, [System.Windows.Automation.Condition]::TrueCondition) |
    Where-Object { $_.Current.Name -eq $name -and $_.Current.ControlType.ProgrammaticName -match $kind } |
    Select-Object -First 1
  if (-not $el) { return "not found (live): $name" }

  try {
    $p = $el.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
    $p.Toggle()
    return "toggled: $name -> $($p.Current.ToggleState)"
  } catch {}
  try {
    $p = $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $p.Invoke()
    return "invoked: $name"
  } catch {}
  if (Click-Element $el) { return "clicked: $name" }
  return "could not act on: $name"
}

foreach ($t in $Toggle) { Write-Output (Invoke-Named $t "CheckBox|RadioButton") ; Start-Sleep -Milliseconds 400 }
foreach ($b in $Press) { Write-Output (Invoke-Named $b "Button") ; Start-Sleep -Milliseconds 800 }

Write-Output "done"
