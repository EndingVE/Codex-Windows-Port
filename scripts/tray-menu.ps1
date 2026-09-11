param([string]$OutDir = (Join-Path $PSScriptRoot '..\evidence'))
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class TM3 {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  public const uint RD = 0x0008, RU = 0x0010, LD = 0x0002, LU = 0x0004;
  public static void Move(int x, int y) { SetCursorPos(x, y); }
  public static void RClick() { mouse_event(RD,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(80); mouse_event(RU,0,0,0,IntPtr.Zero); }
  public static void LClick() { mouse_event(LD,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(80); mouse_event(LU,0,0,0,IntPtr.Zero); }
}
"@
$root = [System.Windows.Automation.AutomationElement]::RootElement
$children = [System.Windows.Automation.TreeScope]::Children
$desc = [System.Windows.Automation.TreeScope]::Descendants
function RectOf($el) { $b = $el.Current.BoundingRectangle; New-Object System.Drawing.Rectangle ([int]$b.X),([int]$b.Y),([int]$b.Width),([int]$b.Height) }
function SaveRect($path, $rect) {
  if ($rect.Width -le 0 -or $rect.Height -le 0) { Write-Output "empty rect"; return }
  $bmp = New-Object System.Drawing.Bitmap $rect.Width, $rect.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($rect.Left,$rect.Top,0,0,(New-Object System.Drawing.Size $rect.Width,$rect.Height)); $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
  Write-Output "saved $path ($($rect.Width)x$($rect.Height))"
}
function Menus { @($root.FindAll($children, [System.Windows.Automation.Condition]::TrueCondition) | Where-Object { try { $_.Current.ClassName -eq "#32768" } catch { $false } }) }

$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, "Shell_TrayWnd")
$tb = $root.FindFirst($children, $cond)
$chev = $null
foreach ($e in $tb.FindAll($desc, [System.Windows.Automation.Condition]::TrueCondition)) {
  $n = ""; try { $n = $e.Current.Name } catch {}
  if ($n -eq "Mostrar iconos ocultos" -or $n -eq "Show hidden icons") { $chev = $e; break }
}
$cr = RectOf $chev
[TM3]::Move(($cr.Left + 16), ($cr.Top + 19)); Start-Sleep -Milliseconds 250
[TM3]::LClick(); Start-Sleep -Milliseconds 1400

$fc = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, "TopLevelWindowForOverflowXamlIsland")
$fly = $root.FindFirst($children, $fc)
if (-not $fly) { Write-Output "no flyout"; exit 2 }
$icon = $null
foreach ($e in $fly.FindAll($desc, [System.Windows.Automation.Condition]::TrueCondition)) {
  $n = ""; try { $n = $e.Current.Name } catch {}
  if ($n -match "Codex") { $icon = $e; break }
}
if (-not $icon) { Write-Output "no codex icon"; exit 3 }
$ir = RectOf $icon
[TM3]::Move(($ir.Left + [int]($ir.Width/2)), ($ir.Top + [int]($ir.Height/2))); Start-Sleep -Milliseconds 700
[TM3]::Move(($ir.Left + [int]($ir.Width/2) + 1), ($ir.Top + [int]($ir.Height/2))); Start-Sleep -Milliseconds 350
[TM3]::RClick(); Start-Sleep -Milliseconds 1400

$menus = Menus
if ($menus.Count -eq 0) { Write-Output "FAIL: no menu window"; exit 4 }
$mr = RectOf $menus[0]
Write-Output "menu rect: $($mr.X),$($mr.Y) $($mr.Width)x$($mr.Height)"
SaveRect (Join-Path $OutDir "tray-menu.png") (New-Object System.Drawing.Rectangle (($mr.Left-6), ($mr.Top-6), ($mr.Width+12), ($mr.Height+12)))

# --- walk the rows to expand the submenu --------------------------------------
$found = $false
for ($step = 0; $step -lt 26 -and -not $found; $step++) {
  $y = $mr.Top + 6 + ($step * 9)
  if ($y -gt $mr.Bottom - 4) { break }
  [TM3]::Move(($mr.Right - 24), $y)
  Start-Sleep -Milliseconds 650
  $m2 = Menus
  if ($m2.Count -ge 2) {
    $sr = RectOf $m2[1]
    if ($sr.Width -gt 40) {
      Write-Output "submenu opened at row ${row}: $($sr.X),$($sr.Y) $($sr.Width)x$($sr.Height)"
      $left = [Math]::Min($mr.Left, $sr.Left)
      $top = [Math]::Min($mr.Top, $sr.Top)
      $right = [Math]::Max($mr.Right, $sr.Right) + 340
      $bottom = [Math]::Max($mr.Bottom, $sr.Bottom)
      SaveRect (Join-Path $OutDir "tray-menu-submenu.png") (New-Object System.Drawing.Rectangle ($left-8), ($top-8), ($right-$left+16), ($bottom-$top+16))
      $found = $true
    }
  }
}
if (-not $found) { Write-Output "no submenu opened during the row scan" }

[TM3]::Move(1200, 200)
Start-Sleep -Milliseconds 200
[TM3]::LClick()
Write-Output "done"
