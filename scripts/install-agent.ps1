# Device HUB をユーザー領域へ入れ、常駐エージェントをログオン時に自動起動する。
#
#   ./scripts/install-agent.ps1            # target/release/device-hub.exe を導入して起動
#   ./scripts/install-agent.ps1 -Uninstall # 自動起動を外してエージェントを止める
#
# 管理者権限は不要 (HKCU の Run キーと %LOCALAPPDATA% だけを使う)。
param(
    [switch]$Uninstall,
    [string]$Source = (Join-Path $PSScriptRoot '..\target\release\device-hub.exe')
)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub'
$exe = Join-Path $dir 'device-hub.exe'
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$name = 'DeviceHubAgent'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Device HUB.lnk'

function Stop-Agent {
    Get-CimInstance Win32_Process -Filter "Name='device-hub.exe'" |
        Where-Object { $_.CommandLine -match '--agent' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
}

if ($Uninstall) {
    Remove-ItemProperty -Path $run -Name $name -ErrorAction SilentlyContinue
    Stop-Agent
    Remove-Item $lnk -ErrorAction SilentlyContinue
    Write-Host "自動起動を解除しました ($exe は残しています)"
    return
}

if (-not (Test-Path $Source)) { throw "ビルド済みの exe がありません: $Source (cargo build --release -p device-hub)" }
New-Item -ItemType Directory -Force $dir | Out-Null
Stop-Agent
Start-Sleep -Milliseconds 300
Copy-Item $Source $exe -Force

Set-ItemProperty -Path $run -Name $name -Value "`"$exe`" --agent"

$sh = New-Object -ComObject WScript.Shell
$s = $sh.CreateShortcut($lnk)
$s.TargetPath = $exe
$s.WorkingDirectory = $dir
$s.Description = 'Device HUB'
$s.Save()

Start-Process -FilePath $exe -ArgumentList '--agent' -WindowStyle Hidden
Write-Host "導入しました: $exe"
Write-Host "ログオン時に '$exe --agent' を自動起動します (HKCU\...\Run\$name)"
