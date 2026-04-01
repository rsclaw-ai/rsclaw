# rsclaw system tray controller for Windows
# Usage: powershell -WindowStyle Hidden -File rsclaw-tray.ps1

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$icon = [System.Drawing.SystemIcons]::Application
$tray = New-Object System.Windows.Forms.NotifyIcon
$tray.Icon = $icon
$tray.Text = "RsClaw Gateway"
$tray.Visible = $true

# --- Menu items ---
$menu = New-Object System.Windows.Forms.ContextMenuStrip

$statusItem = $menu.Items.Add("Status: checking...")
$statusItem.Enabled = $false
$menu.Items.Add("-")  # separator

$startItem = $menu.Items.Add("Start Gateway")
$startItem.Add_Click({
    Start-Process -NoNewWindow -FilePath "rsclaw" -ArgumentList "gateway", "start"
    Update-Status
})

$stopItem = $menu.Items.Add("Stop Gateway")
$stopItem.Add_Click({
    Start-Process -NoNewWindow -FilePath "rsclaw" -ArgumentList "gateway", "stop"
    Start-Sleep -Seconds 1
    Update-Status
})

$restartItem = $menu.Items.Add("Restart Gateway")
$restartItem.Add_Click({
    Start-Process -NoNewWindow -FilePath "rsclaw" -ArgumentList "gateway", "stop" -Wait
    Start-Sleep -Milliseconds 500
    Start-Process -NoNewWindow -FilePath "rsclaw" -ArgumentList "gateway", "start"
    Start-Sleep -Seconds 1
    Update-Status
})

$menu.Items.Add("-")

$logsItem = $menu.Items.Add("View Logs")
$logsItem.Add_Click({
    Start-Process -FilePath "rsclaw" -ArgumentList "logs", "--follow"
})

$doctorItem = $menu.Items.Add("Doctor")
$doctorItem.Add_Click({
    $result = & rsclaw doctor 2>&1 | Out-String
    [System.Windows.Forms.MessageBox]::Show($result, "RsClaw Doctor", "OK", "Information")
})

$configItem = $menu.Items.Add("Open Config")
$configItem.Add_Click({
    $configPath = Join-Path $env:LOCALAPPDATA "rsclaw\rsclaw.json5"
    if (-not (Test-Path $configPath)) {
        $configPath = Join-Path $env:USERPROFILE ".rsclaw\rsclaw.json5"
    }
    if (Test-Path $configPath) {
        Start-Process notepad $configPath
    } else {
        [System.Windows.Forms.MessageBox]::Show("Config not found. Run 'rsclaw setup' first.", "RsClaw")
    }
})

$menu.Items.Add("-")

$versionItem = $menu.Items.Add("Version")
$versionItem.Add_Click({
    $ver = & rsclaw --version 2>&1 | Out-String
    [System.Windows.Forms.MessageBox]::Show($ver.Trim(), "RsClaw Version", "OK", "Information")
})

$exitItem = $menu.Items.Add("Exit")
$exitItem.Add_Click({
    $tray.Visible = $false
    $tray.Dispose()
    [System.Windows.Forms.Application]::Exit()
})

$tray.ContextMenuStrip = $menu

# --- Status check ---
function Update-Status {
    try {
        $output = & rsclaw gateway status 2>&1 | Out-String
        if ($output -match "running") {
            $statusItem.Text = "Status: Running"
            $tray.Text = "RsClaw Gateway (Running)"
            $startItem.Enabled = $false
            $stopItem.Enabled = $true
            $restartItem.Enabled = $true
        } else {
            $statusItem.Text = "Status: Stopped"
            $tray.Text = "RsClaw Gateway (Stopped)"
            $startItem.Enabled = $true
            $stopItem.Enabled = $false
            $restartItem.Enabled = $false
        }
    } catch {
        $statusItem.Text = "Status: Unknown"
        $tray.Text = "RsClaw Gateway"
    }
}

# --- Auto refresh timer ---
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 10000  # 10 seconds
$timer.Add_Tick({ Update-Status })
$timer.Start()

# --- Double-click opens status ---
$tray.Add_DoubleClick({ Update-Status })

# Initial status check
Update-Status

# Run message loop
[System.Windows.Forms.Application]::Run()
