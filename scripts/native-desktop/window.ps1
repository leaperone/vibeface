param([Parameter(Mandatory=$true)][int]$OwnedPid, [Parameter(Mandatory=$true)][string]$Screenshot)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class NativeDesktopWindow {
  public delegate bool WindowVisitor(IntPtr window, IntPtr parameter);
  [StructLayout(LayoutKind.Sequential)] public struct Rectangle { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(WindowVisitor visitor, IntPtr parameter);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rectangle rect);
}
'@
$script:windowHandle = [IntPtr]::Zero
[NativeDesktopWindow]::EnumWindows({
  param($window, $unused)
  [uint32]$owner = 0
  [void][NativeDesktopWindow]::GetWindowThreadProcessId($window, [ref]$owner)
  if ($owner -eq $OwnedPid -and [NativeDesktopWindow]::IsWindowVisible($window)) { $script:windowHandle = $window; return $false }
  return $true
}, [IntPtr]::Zero) | Out-Null
if ($script:windowHandle -eq [IntPtr]::Zero) { throw 'No visible native window belongs to the driver-owned PID' }
if (-not [NativeDesktopWindow]::SetForegroundWindow($script:windowHandle)) { throw 'The native window could not receive keyboard focus' }
[System.Windows.Forms.SendKeys]::SendWait('native-gpui-fixture')
[System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
$rect = New-Object NativeDesktopWindow+Rectangle
if (-not [NativeDesktopWindow]::GetWindowRect($script:windowHandle, [ref]$rect)) { throw 'Cannot read native window bounds' }
$bitmap = New-Object System.Drawing.Bitmap ($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {
  $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
  $bitmap.Save($Screenshot, [System.Drawing.Imaging.ImageFormat]::Png)
} finally { $graphics.Dispose(); $bitmap.Dispose() }
@{pid = $OwnedPid; window_id = $script:windowHandle.ToInt64(); keyboard = 'SendKeys'; screenshot = $Screenshot} | ConvertTo-Json -Compress
