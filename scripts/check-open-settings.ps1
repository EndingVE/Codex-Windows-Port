param([string]$OutDir = (Join-Path $PSScriptRoot '..\evidence'))
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$children = [System.Windows.Automation.TreeScope]::Children
$desc = [System.Windows.Automation.TreeScope]::Descendants
function FindWin([string]$t) {
  $c = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, $t)
  return $root.FindFirst($children, $c)
}
$pop = FindWin "CodexBar"
if (-not $pop) { Write-Output "FAIL: popover window not found"; exit 2 }
$found = $false
foreach ($e in $pop.FindAll($desc, [System.Windows.Automation.Condition]::TrueCondition)) {
  $n = ""; try { $n = $e.Current.Name } catch {}
  $ct = ""; try { $ct = $e.Current.ControlType.ProgrammaticName } catch {}
  if ($ct -ne "ControlType.Button") { continue }
  if ($n -notmatch "Settings") { continue }
  Write-Output "popover button: '$n'"
  try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); Write-Output "invoked"; $found = $true } catch { Write-Output "invoke failed: $_" }
  break
}
if (-not $found) { Write-Output "no Settings button in the popover"; exit 3 }
Start-Sleep -Seconds 3
$win = FindWin "CodexBar Settings"
if ($win) {
  Write-Output "OK: open_settings from the popover produced the 'CodexBar Settings' window (visible=$($win.Current.IsOffscreen -eq $false))"
} else {
  Write-Output "FAIL: no settings window after invoking the popover button"
}
