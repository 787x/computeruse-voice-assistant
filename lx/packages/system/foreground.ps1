# 取前台窗口矩形与所在显示器工作区，供全屏判定使用。
# 不依赖 System.Windows.Forms：某些宿主里 Add-Type -AssemblyName 会 TypeNotFound。
# 注意：PowerShell 的 here-string 终止符 "@ 必须**独占一行**，后面不能跟参数，
# 所以先赋给变量再传给 Add-Type（踩过一次：写成 `"@ -ErrorAction Stop` 会让 parser 崩）。
$code = @"
using System;
using System.Runtime.InteropServices;
public class FgWin {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);
  [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr monitor, ref MONITORINFO info);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO {
    public int cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags;
  }
}
"@

Add-Type -TypeDefinition $code -ErrorAction Stop

$hwnd = [FgWin]::GetForegroundWindow()
$rect = New-Object FgWin+RECT
[void][FgWin]::GetWindowRect($hwnd, [ref]$rect)

$procId = [uint32]0
[void][FgWin]::GetWindowThreadProcessId($hwnd, [ref]$procId)
$proc = Get-Process -Id $procId -ErrorAction SilentlyContinue

$info = New-Object FgWin+MONITORINFO
$info.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf($info)
$monitor = [FgWin]::MonitorFromWindow($hwnd, 2)
$null = [FgWin]::GetMonitorInfo($monitor, [ref]$info)

[pscustomobject]@{
  hwnd       = ('0x{0:x}' -f $hwnd.ToInt64())
  title      = if ($proc) { $proc.MainWindowTitle } else { '' }
  process    = if ($proc) { $proc.ProcessName } else { '' }
  pid        = [int]$procId
  maximized  = [bool][FgWin]::IsZoomed($hwnd)
  left       = $rect.Left
  top        = $rect.Top
  right      = $rect.Right
  bottom     = $rect.Bottom
  screenLeft = $info.rcWork.Left
  screenTop  = $info.rcWork.Top
  screenW    = ($info.rcWork.Right - $info.rcWork.Left)
  screenH    = ($info.rcWork.Bottom - $info.rcWork.Top)
  monLeft    = $info.rcMonitor.Left
  monTop     = $info.rcMonitor.Top
  monW       = ($info.rcMonitor.Right - $info.rcMonitor.Left)
  monH       = ($info.rcMonitor.Bottom - $info.rcMonitor.Top)
} | ConvertTo-Json -Compress
