param([string]$Exe, [string]$Arguments = '', [string]$Steps, [string]$OutDir, [string]$LocalAppData)
# Dev check of the app window. Starts it without focus (SW_SHOWNOACTIVATE) and without a console window, stdout and
# stderr going to <OutDir>\app.log, with LOCALAPPDATA (WebView2 data) at a scratch dir, and runs steps with posted
# messages (the real mouse and keyboard are not touched):
#   powershell -File scripts/drive.ps1 -Exe target/debug/mailpro.exe -Arguments tests/privacy/tracking.eml -Steps "wait 3000; shot a" -OutDir <dir> -LocalAppData <scratch dir>
# Steps, separated by ';': wait <ms> | click <x> <y> | wheel <x> <y> <notches> | type <ascii, no spaces> | shot <name>
# Coordinates are client DIPs of the main window; clicks and wheel go to the deepest child window under the
# point, so they reach the WebView2 body. The process tree started here is killed at the end.
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class D {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct SI {
    public int cb; public string r, desk, title; public int x, y, w, h, xc, yc, fill, flags; public short show, r2; public IntPtr r3, i, o, e; }
  [StructLayout(LayoutKind.Sequential)] public struct PI { public IntPtr hp, ht; public int pid, tid; }
  [StructLayout(LayoutKind.Sequential)] public struct SA { public int n; public IntPtr d; public bool inherit; }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool CreateProcessW(string app, string cmd, IntPtr pa, IntPtr ta, bool inherit, int flags, IntPtr env, string dir, ref SI si, out PI pi);
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)] public static extern IntPtr CreateFileW(string name, uint access, uint share, ref SA sa, uint disp, uint flags, IntPtr t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern IntPtr ChildWindowFromPointEx(IntPtr h, POINT p, uint f);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@
$env:LOCALAPPDATA = $LocalAppData
$si = New-Object D+SI
$sa = New-Object D+SA; $sa.n = [Runtime.InteropServices.Marshal]::SizeOf($sa); $sa.inherit = $true
# GENERIC_WRITE, share read/write, CREATE_ALWAYS
$log = [D]::CreateFileW((Join-Path $OutDir 'app.log'), 0x40000000, 3, [ref]$sa, 2, 0x80, [IntPtr]::Zero)
# STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES, SW_SHOWNOACTIVATE
$si.cb = [Runtime.InteropServices.Marshal]::SizeOf($si); $si.flags = 0x101; $si.show = 4; $si.o = $log; $si.e = $log
$pi = New-Object D+PI
# CREATE_NO_WINDOW: the debug build is a console program
if (-not [D]::CreateProcessW($Exe, "`"$Exe`" $Arguments", [IntPtr]::Zero, [IntPtr]::Zero, $true, 0x08000000, [IntPtr]::Zero, [NullString]::Value, [ref]$si, [ref]$pi)) { "CreateProcess failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"; exit 1 }
$p = Get-Process -Id $pi.pid
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($p.MainWindowHandle -eq 0 -and $sw.ElapsedMilliseconds -lt 15000) { Start-Sleep -Milliseconds 50; $p.Refresh() }
$h = $p.MainWindowHandle
Start-Sleep -Milliseconds 400
$s = [D]::GetDpiForWindow($h) / 96.0
"pid $($pi.pid) scale $s foreground-is-ours $([D]::GetForegroundWindow() -eq $h)"
function MakeLParam($x, $y) { [IntPtr](([int]$y -shl 16) -bor ([int]$x -band 0xffff)) }
# Deepest visible child under a client DIP point of the main window, and the point in its client coordinates
function Target($x, $y) {
  $pt = New-Object D+POINT; $pt.X = [int]([double]$x * $s); $pt.Y = [int]([double]$y * $s)
  [void][D]::ClientToScreen($h, [ref]$pt)
  $w = $h
  while ($true) {
    $c = New-Object D+POINT; $c.X = $pt.X; $c.Y = $pt.Y; [void][D]::ScreenToClient($w, [ref]$c)
    $next = [D]::ChildWindowFromPointEx($w, $c, 1)
    if ($next -eq [IntPtr]::Zero -or $next -eq $w) { return @($w, $c, $pt) }
    $w = $next
  }
}
foreach ($step in $Steps.Split(';')) {
  $a = $step.Trim().Split(' ')
  switch ($a[0]) {
    'wait' { Start-Sleep -Milliseconds ([int]$a[1]) }
    'click' {
      $t = Target $a[1] $a[2]; $l = MakeLParam $t[1].X $t[1].Y
      [void][D]::PostMessage($t[0], 0x200, 0, $l); Start-Sleep -Milliseconds 60
      [void][D]::PostMessage($t[0], 0x201, 1, $l); Start-Sleep -Milliseconds 40
      [void][D]::PostMessage($t[0], 0x202, 0, $l); Start-Sleep -Milliseconds 400
    }
    'wheel' {
      $t = Target $a[1] $a[2]
      [void][D]::PostMessage($t[0], 0x20A, [IntPtr]([int]$a[3] * 120 -shl 16), (MakeLParam $t[2].X $t[2].Y)); Start-Sleep -Milliseconds 300
    }
    'type' { foreach ($ch in $a[1].ToCharArray()) { [void][D]::PostMessage($h, 0x102, [int]$ch, 1); Start-Sleep -Milliseconds 20 } }
    'shot' {
      $r = New-Object D+RECT
      [void][D]::GetWindowRect($h, [ref]$r)
      $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
      $g = [Drawing.Graphics]::FromImage($bmp); $dc = $g.GetHdc()
      [void][D]::PrintWindow($h, $dc, 2); $g.ReleaseHdc($dc)
      $bmp.Save((Join-Path $OutDir "$($a[1]).png"))
    }
  }
}
if (-not $p.HasExited) { & taskkill.exe /PID $pi.pid /T /F | Out-Null }
