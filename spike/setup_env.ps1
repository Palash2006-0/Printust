<#
.SYNOPSIS
    Idempotent environment setup for PrintLedger Phase 1 (Days 1 & 2).
.DESCRIPTION
    1. Verifies Administrator privileges.
    2. Enables and verifies Microsoft-Windows-PrintService/Operational event logging.
    3. Lists installed printer drivers (and offers inbox PCL6 install if none is present).
    4. Ensures local port "NUL" exists.
    5. Guides or creates the silent "ShopTest_PCL6" printer bound to port "NUL".
#>
[CmdletBinding()]
param(
    [string]$DriverName = "",
    [switch]$AutoBind
)

$ErrorActionPreference = "Stop"

# 1. Check Administrator privileges
$currentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($currentIdentity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $repoRoot = Split-Path -Parent $PSScriptRoot
    Write-Warning "Administrator privileges are required to configure event logs, printer ports, and printers."
    Write-Host ""
    Write-Host "Please open PowerShell as Administrator and re-run:" -ForegroundColor Yellow
    Write-Host "  cd `"$repoRoot`""
    Write-Host "  Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass"
    Write-Host "  .\spike\setup_env.ps1"
    exit 1
}

Write-Host "=== PrintLedger Phase 1: Environment Setup ===" -ForegroundColor Cyan

# 2. Enable Windows PrintService Operational log
Write-Host "`n[1/4] Enabling Microsoft-Windows-PrintService/Operational event log..." -ForegroundColor Cyan
wevtutil sl Microsoft-Windows-PrintService/Operational /e:true
if ($LASTEXITCODE -ne 0) {
    throw "Failed to enable Microsoft-Windows-PrintService/Operational log (exit code $LASTEXITCODE)."
}

# 3. Verify logging is active
$logConfig = wevtutil gl Microsoft-Windows-PrintService/Operational
$logConfig | ForEach-Object { Write-Host "  $_" }
if (-not ($logConfig -match "enabled:\s*true")) {
    throw "Microsoft-Windows-PrintService/Operational did not report 'enabled: true'."
}
Write-Host "  [OK] Event logging is active." -ForegroundColor Green

# 4. List installed printer drivers
Write-Host "`n[2/4] Checking installed local printer drivers..." -ForegroundColor Cyan
$drivers = @(Get-PrinterDriver)
if ($drivers.Count -eq 0) {
    Write-Warning "No printer drivers found."
} else {
    $drivers | Select-Object Name, MajorVersion, Manufacturer | Format-Table -AutoSize | Out-String | Write-Host
}

$preferredPatterns = @(
    "HP Universal Printing PCL 6*",
    "Microsoft PCL6 Class Driver",
    "Microsoft PS Class Driver",
    "Canon Generic Plus*",
    "*PCL6*",
    "*PostScript*"
)

$matchingDrivers = @()
foreach ($pattern in $preferredPatterns) {
    $hits = @($drivers | Where-Object { $_.Name -like $pattern })
    foreach ($hit in $hits) {
        if ($matchingDrivers.Name -notcontains $hit.Name) {
            $matchingDrivers += $hit
        }
    }
}

if ($matchingDrivers.Count -eq 0) {
    Write-Warning "No PCL6 or PostScript driver is currently registered in Spooler."
    $prnms005 = Get-ChildItem "$env:SystemRoot\System32\DriverStore\FileRepository\prnms005.inf*\prnms005.inf" -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName

    if ($prnms005) {
        Write-Host "  Found Windows inbox PCL6/PS driver package: $prnms005" -ForegroundColor Yellow
        $installAnswer = Read-Host "  Install 'Microsoft PCL6 Class Driver' from Windows DriverStore now? [Y/n]"
        if ([string]::IsNullOrWhiteSpace($installAnswer) -or $installAnswer -match "^[Yy]") {
            try {
                Add-PrinterDriver -Name "Microsoft PCL6 Class Driver"
            } catch {
                Write-Host "  Retrying Add-PrinterDriver with explicit -InfPath..." -ForegroundColor Yellow
                Add-PrinterDriver -Name "Microsoft PCL6 Class Driver" -InfPath $prnms005
            }
            $drivers = @(Get-PrinterDriver)
            $matchingDrivers = @($drivers | Where-Object { $_.Name -eq "Microsoft PCL6 Class Driver" })
            Write-Host "  [OK] Installed 'Microsoft PCL6 Class Driver'." -ForegroundColor Green
        }
    } else {
        Write-Host "  Install HP Universal Print Driver (PCL6) or add Microsoft PCL6/PS Class Driver via Control Panel -> Devices and Printers." -ForegroundColor Yellow
    }
} else {
    Write-Host "  [OK] Detected PCL6/PS driver candidate(s): $($matchingDrivers.Name -join ', ')" -ForegroundColor Green
}

# 5. Check / create local port named NUL
Write-Host "`n[3/4] Checking local printer port 'NUL'..." -ForegroundColor Cyan
$nulPort = Get-PrinterPort -Name "NUL" -ErrorAction SilentlyContinue
if ($null -ne $nulPort) {
    Write-Host "  [OK] Local port 'NUL' already exists." -ForegroundColor Green
} else {
    Add-PrinterPort -Name "NUL"
    Write-Host "  [OK] Created local port 'NUL'." -ForegroundColor Green
}

# 6. Check / guide ShopTest_PCL6 printer creation
Write-Host "`n[4/4] Checking test printer 'ShopTest_PCL6'..." -ForegroundColor Cyan
$existingPrinter = Get-Printer -Name "ShopTest_PCL6" -ErrorAction SilentlyContinue
if ($null -ne $existingPrinter) {
    Write-Host "  [OK] Printer 'ShopTest_PCL6' already exists:" -ForegroundColor Green
    $existingPrinter | Select-Object Name, DriverName, PortName | Format-Table -AutoSize | Out-String | Write-Host
} else {
    $suggestedDriver = if (-not [string]::IsNullOrWhiteSpace($DriverName)) {
        $DriverName
    } elseif ($matchingDrivers.Count -gt 0) {
        $matchingDrivers[0].Name
    } else {
        "<SELECTED_DRIVER_NAME>"
    }

    Write-Host "  Guided command to bind 'ShopTest_PCL6' to port 'NUL':" -ForegroundColor Yellow
    Write-Host "    Add-Printer -Name `"ShopTest_PCL6`" -DriverName `"$suggestedDriver`" -PortName `"NUL`"" -ForegroundColor White

    if ($suggestedDriver -ne "<SELECTED_DRIVER_NAME>") {
        $shouldBind = $AutoBind
        if (-not $shouldBind) {
            $bindAnswer = Read-Host "`n  Bind 'ShopTest_PCL6' using driver '$suggestedDriver' on port 'NUL' now? [Y/n]"
            $shouldBind = ([string]::IsNullOrWhiteSpace($bindAnswer) -or $bindAnswer -match "^[Yy]")
        }
        if ($shouldBind) {
            Add-Printer -Name "ShopTest_PCL6" -DriverName $suggestedDriver -PortName "NUL"
            Write-Host "  [OK] Bound 'ShopTest_PCL6' -> '$suggestedDriver' (Port: NUL)." -ForegroundColor Green
            Get-Printer -Name "ShopTest_PCL6" | Select-Object Name, DriverName, PortName | Format-Table -AutoSize | Out-String | Write-Host
        }
    }
}

Write-Host "`n=== Next Step: Smoke Test & Watcher ===" -ForegroundColor Cyan
Write-Host "1. Start watcher: .\spike\.venv\Scripts\python.exe .\spike\spooler_spike.py"
Write-Host "2. Print test PDFs to 'ShopTest_PCL6' and verify Event IDs 307 & 805:"
Write-Host '   Get-WinEvent -LogName "Microsoft-Windows-PrintService/Operational" -MaxEvents 10 | Where-Object { $_.Id -in 307, 805 } | Format-List TimeCreated, Id, RecordId, Message'
