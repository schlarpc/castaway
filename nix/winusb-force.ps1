# Force the already-installed castaway WinUSB package onto the radio.
#
# `pnputil /add-driver /install` respects driver *ranking*, and ranking is decided first by
# signature class: a WHQL-signed inbox driver beats a self-signed package no matter what its
# DriverVer says. Intel's BTHUSB will therefore win that comparison every time, which is why
# the install reported "up-to-date on device" and changed nothing.
#
# The documented way past that is UpdateDriverForPlugAndPlayDevices with INSTALLFLAG_FORCE,
# which skips ranking and binds the INF you name. It is what devcon's `update` verb does and
# what Zadig does internally; there is no pnputil equivalent.
param(
  [string]$HardwareId = 'USB\VID_8087&PID_0033',
  [string]$InfPath = "$env:TEMP\castaway-winusb\castaway-winusb.inf"
)
$ErrorActionPreference = 'Stop'
function Say($m) { Write-Output ("==> " + $m) }

if (-not (Test-Path $InfPath)) { throw "no INF at $InfPath -- run winusb-bind.ps1 first" }

Add-Type -Namespace Castaway -Name NewDev -MemberDefinition @'
[DllImport("newdev.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern bool UpdateDriverForPlugAndPlayDevicesW(
    IntPtr hwndParent, string HardwareId, string FullInfPath,
    uint InstallFlags, out bool bRebootRequired);
'@

function Show-State($label) {
  Say $label
  Get-PnpDevice -PresentOnly | Where-Object {
    $hw = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' -ErrorAction SilentlyContinue).Data
    $hw -and ($hw -contains $HardwareId)
  } | ForEach-Object {
    $svc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
    $inf = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_DriverInfPath' -ErrorAction SilentlyContinue).Data
    Write-Output ("    {0}  status={1}  service={2}  inf={3}" -f $_.InstanceId, $_.Status, $svc, $inf)
  }
}

Show-State "before"

$INSTALLFLAG_FORCE = 0x00000001
$reboot = $false
Say "forcing $InfPath onto $HardwareId"
$ok = [Castaway.NewDev]::UpdateDriverForPlugAndPlayDevicesW(
        [IntPtr]::Zero, $HardwareId, $InfPath, $INSTALLFLAG_FORCE, [ref]$reboot)
if (-not $ok) {
  $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
  # 0xE000020B / 3010 etc. are informational; anything else is a real refusal.
  Say ("UpdateDriverForPlugAndPlayDevices failed, GetLastError=0x{0:X8} ({0})" -f $err)
  switch ($err) {
    0xE0000203 { Say "  ERROR_NO_SUCH_DEVINST -- no device with that hardware id is present" }
    0xE0000217 { Say "  ERROR_NO_DRIVER_SELECTED / no matching device for this INF" }
    0xE0000242 { Say "  ERROR_NO_CATALOG_FOR_OEM_INF -- the catalog is not trusted" }
    0x00000005 { Say "  ACCESS DENIED -- this needs an elevated session" }
    default    { Say "  (see setupapi.dev.log for the ranking/refusal detail)" }
  }
} else {
  Say ("forced; rebootRequired=" + $reboot)
}

Start-Sleep -Seconds 5
Show-State "after"

$svc = Get-PnpDevice -PresentOnly | Where-Object {
  $hw = (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' -ErrorAction SilentlyContinue).Data
  $hw -and ($hw -contains $HardwareId)
} | ForEach-Object {
  (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
}
if ($svc -contains 'WinUSB') { Say "SUCCESS: the radio is on WinUSB"; exit 0 }
Say "still not WinUSB (service=$svc)"
exit 1
