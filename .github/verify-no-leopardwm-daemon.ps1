#Requires -Version 5.1
# Refuse, without terminating anything, to run owned-HWND integration probes while
# a LeopardWM daemon or watchdog could manage the probe's temporary windows.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$processes = @(Get-Process -Name 'leopardwm', 'leopardwm-watchdog' -ErrorAction SilentlyContinue)
if ($processes.Count -gt 0) {
    $details = $processes | ForEach-Object { "$($_.ProcessName) (PID $($_.Id))" }
    throw "Refusing to run controlled Win32 probes while LeopardWM is running: $($details -join ', ')"
}

if (-not ('LeopardWMProbePreflight' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class LeopardWMProbePreflight
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr FindWindow(string className, string windowName);
}
'@
}

$daemonWindow = [LeopardWMProbePreflight]::FindWindow('LeopardWMSysEventClass', $null)
if ($daemonWindow -ne [IntPtr]::Zero) {
    throw "Refusing to run controlled Win32 probes while LeopardWMSysEventClass exists (HWND $daemonWindow)"
}

Write-Host 'No LeopardWM daemon or watchdog is running; controlled Win32 probes may start.'
