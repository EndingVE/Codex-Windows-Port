# screenshot-window.ps1 — capture a top-level window to PNG.
#
# Used for build evidence: launches nothing itself, just finds a window by
# process name and grabs it. Tries PrintWindow(PW_RENDERFULLCONTENT) first, which
# works for composited windows, and falls back to a screen grab of the window
# rect (the WebView2 surface is composited to the screen, so that always works
# when the window is unobstructed).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File screenshot-window.ps1 `
#       -ProcessName codexbar-win -Out C:\path\shot.png

param(
  [Parameter(Mandatory = $true)][string]$ProcessName,
  [Parameter(Mandatory = $true)][string]$Out,
  [int]$WaitSeconds = 25
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinCap {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder s, int n);
}
"@

# --- find the process and a visible, named top-level window -------------------
$deadline = (Get-Date).AddSeconds($WaitSeconds)
$hwnd = [IntPtr]::Zero

# The process also owns two helper windows (a 16x16 "Tao Thread Event Target"
# and the tray's hidden "tray_icon_app"), and .NET's MainWindowHandle happily
# returns one of those. Enumerate instead and pick the real app window.
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinFind {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);

  public class Hit { public IntPtr H; public int Area; public bool Visible; public string Title; public string Cls; }

  public static List<Hit> ForPid(uint want) {
    var found = new List<Hit>();
    EnumWindows((h, l) => {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid != want) return true;
      var t = new StringBuilder(256); GetWindowText(h, t, 256);
      var c = new StringBuilder(256); GetClassName(h, c, 256);
      RECT r; GetWindowRect(h, out r);
      var area = (r.Right - r.Left) * (r.Bottom - r.Top);
      found.Add(new Hit { H = h, Area = area, Visible = IsWindowVisible(h),
                          Title = t.ToString(), Cls = c.ToString() });
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@

$capture = $null
while ((Get-Date) -lt $deadline) {
  foreach ($p in @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)) {
    $hits = [WinFind]::ForPid([uint32]$p.Id) | Where-Object { $_.Title -eq "CodexBar" -or $_.Cls -eq "Tauri Window" }
    $visible = $hits | Where-Object { $_.Visible } | Sort-Object Area -Descending | Select-Object -First 1
    if ($visible) { $capture = $visible; break }
    # Keep the largest hidden candidate as a fallback for the error message.
    if (-not $hwnd -or ($hits | Sort-Object Area -Descending | Select-Object -First 1).Area -gt 0) {
      $best = $hits | Sort-Object Area -Descending | Select-Object -First 1
      if ($best) { $hwnd = $best.H }
    }
  }
  if ($capture) { break }
  Start-Sleep -Milliseconds 400
}

if ($capture) { $hwnd = $capture.H }
if ($hwnd -eq [IntPtr]::Zero) { throw "no app window found for process '$ProcessName' within $WaitSeconds s" }
if (-not $capture) { throw "app window for '$ProcessName' exists but is not visible (handle $hwnd)" }

# Bring it forward so the composited surface is on screen for the fallback path.
[void][WinCap]::ShowWindow($hwnd, 5)
[void][WinCap]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 1200

$rect = New-Object WinCap+RECT
[void][WinCap]::GetWindowRect($hwnd, [ref]$rect)
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
if ($w -le 0 -or $h -le 0) { throw "window has no usable size ($w x $h)" }

$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

# --- attempt 1: PrintWindow with full content --------------------------------
$bmp = New-Object System.Drawing.Bitmap $w, $h
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $gfx.GetHdc()
$ok = [WinCap]::PrintWindow($hwnd, $hdc, 2)
$gfx.ReleaseHdc($hdc)
$gfx.Dispose()

$nonBlank = 0
if ($ok) {
  for ($y = 0; $y -lt $h; $y += [Math]::Max(1, [int]($h / 40))) {
    for ($x = 0; $x -lt $w; $x += [Math]::Max(1, [int]($w / 40))) {
      $p = $bmp.GetPixel($x, $y)
      if ($p.R + $p.G + $p.B -gt 24) { $nonBlank++ }
    }
  }
}

if ($ok -and $nonBlank -gt 40) {
  $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
  "PrintWindow capture ok ($w x $h, $nonBlank bright samples) -> $Out"
} else {
  $bmp.Dispose()
  $bmp2 = New-Object System.Drawing.Bitmap $w, $h
  $gfx2 = [System.Drawing.Graphics]::FromImage($bmp2)
  $gfx2.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
  $gfx2.Dispose()
  $bmp2.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
  "Screen capture fallback ($w x $h) -> $Out"
}
