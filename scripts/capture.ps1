# capture.ps1 — capture the windows of a process, by title regex, to PNG.
#
# Tries PrintWindow(PW_RENDERFULLCONTENT) first (works for the composited
# WebView2 surface) and falls back to a screen grab of the window rect.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File capture.ps1 `
#       -ProcessName codexbar-win -TitleMatch 'CodexBar' -OutDir C:\...\evidence

param(
  [Parameter(Mandatory = $true)][string]$ProcessName,
  [Parameter(Mandatory = $true)][string]$OutDir,
  [string]$TitleMatch = ".*",
  [int]$WaitSeconds = 20
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinGrab {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);

  public class Hit { public IntPtr H; public bool Visible; public string Title = ""; public string Cls = ""; public RECT R; }

  public static List<Hit> Scan(uint want) {
    var found = new List<Hit>();
    EnumWindows((h, l) => {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid != want) return true;
      var t = new StringBuilder(512); GetWindowText(h, t, 512);
      var c = new StringBuilder(512); GetClassName(h, c, 512);
      RECT r; GetWindowRect(h, out r);
      var hit = new Hit(); hit.H = h; hit.Visible = IsWindowVisible(h);
      hit.Title = t.ToString(); hit.Cls = c.ToString(); hit.R = r;
      found.Add(hit);
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@

function Grab([IntPtr]$hwnd, [string]$out) {
  $r = New-Object WinGrab+RECT
  [void][WinGrab]::GetWindowRect($hwnd, [ref]$r)
  $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
  if ($w -le 0 -or $h -le 0) { return $false }

  [void][WinGrab]::ShowWindow($hwnd, 5)
  [void][WinGrab]::SetForegroundWindow($hwnd)
  Start-Sleep -Milliseconds 900

  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc()
  $ok = [WinGrab]::PrintWindow($hwnd, $hdc, 2)
  $g.ReleaseHdc($hdc)
  $g.Dispose()

  $bright = 0
  if ($ok) {
    for ($y = 0; $y -lt $h; $y += [Math]::Max(1, [int]($h / 30))) {
      for ($x = 0; $x -lt $w; $x += [Math]::Max(1, [int]($w / 30))) {
        $p = $bmp.GetPixel($x, $y)
        if ($p.R + $p.G + $p.B -gt 24) { $bright++ }
      }
    }
  }
  if ($ok -and $bright -gt 25) {
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Output "PrintWindow ok ($w x $h) -> $out"
    return $true
  }
  $bmp.Dispose()
  $bmp2 = New-Object System.Drawing.Bitmap $w, $h
  $g2 = [System.Drawing.Graphics]::FromImage($bmp2)
  $g2.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
  $g2.Dispose()
  $bmp2.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp2.Dispose()
  Write-Output "screen fallback ($w x $h) -> $out"
  return $true
}

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }

$deadline = (Get-Date).AddSeconds($WaitSeconds)
$done = @{}
while ((Get-Date) -lt $deadline) {
  foreach ($p in @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)) {
    foreach ($hit in [WinGrab]::Scan([uint32]$p.Id)) {
      if (-not $hit.Title -or -not $hit.Visible) { continue }
      if ($hit.Title -notmatch $TitleMatch) { continue }
      if ($done.ContainsKey($hit.Title)) { continue }
      $slug = ($hit.Title -replace '[^A-Za-z0-9]+', '-').Trim('-').ToLower()
      $out = Join-Path $OutDir "$slug.png"
      if (Grab $hit.H $out) { $done[$hit.Title] = $true }
    }
  }
  if ($done.Count -ge 2) { break }
  Start-Sleep -Milliseconds 500
}

Write-Output "captured: $($done.Keys -join ', ')"
if ($done.Count -eq 0) { Write-Output "FAIL: no visible window matched '$TitleMatch' for process '$ProcessName'"; exit 2 }
