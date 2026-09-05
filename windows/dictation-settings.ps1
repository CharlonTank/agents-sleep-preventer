$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[Windows.Forms.Application]::EnableVisualStyles()
$settingsPath = Join-Path $env:ASP_DATA_DIR 'dictation.json'
$settings = @{ enabled = $true; model = 'parakeet-v3'; language = 'en'; hotkey = 'Ctrl+Alt+Space'; vocabulary = ''; sounds = $true }
if (Test-Path -LiteralPath $settingsPath) {
    try { $settings = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json } catch { }
}
$form = New-Object Windows.Forms.Form
$form.Text = 'Agents Sleep Preventer - Dictation'
$form.ClientSize = New-Object Drawing.Size(490, 590)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedDialog'
$form.MaximizeBox = $false
$form.Font = New-Object Drawing.Font('Segoe UI', 10)

function Add-Label([string]$text, [int]$y, [int]$height = 24) {
    $label = New-Object Windows.Forms.Label
    $label.Text = $text; $label.Location = New-Object Drawing.Point(20, $y)
    $label.Size = New-Object Drawing.Size(450, $height); $form.Controls.Add($label)
}
Add-Label 'Local dictation with Whisper or Parakeet. Your audio stays on this PC.' 18 44
$enabled = New-Object Windows.Forms.CheckBox
$enabled.Text = 'Enable dictation'; $enabled.Checked = $settings.enabled
$enabled.Location = New-Object Drawing.Point(20, 65); $enabled.Size = New-Object Drawing.Size(400, 28)
$form.Controls.Add($enabled)
Add-Label 'Speech model' 105
$model = New-Object Windows.Forms.ComboBox
$model.DropDownStyle = 'DropDownList'; $model.Location = New-Object Drawing.Point(20, 132); $model.Width = 450
$modelIds = @('large-v3-turbo-q5_0', 'parakeet-v3', 'tiny')
$model.Items.AddRange(@('Whisper Turbo - best accuracy (574 MB)', 'Parakeet v3 - fast multilingual (669 MB)', 'Whisper Tiny - lightweight (78 MB)'))
$model.SelectedIndex = [Math]::Max(0, [Array]::IndexOf($modelIds, [string]$settings.model)); $form.Controls.Add($model)
Add-Label 'After saving, choose Download Dictation Model in the tray menu.' 165 40
Add-Label 'Language (auto = automatic detection)' 208
$language = New-Object Windows.Forms.ComboBox
$language.Items.AddRange(@('en', 'fr', 'auto', 'de', 'es', 'it', 'pt', 'ja', 'zh', 'ko', 'uk', 'ru'))
$language.Text = $settings.language; $language.Location = New-Object Drawing.Point(20, 235); $language.Width = 160
$form.Controls.Add($language)
Add-Label 'Shortcut - press once to start, once to finish' 275
$hotkey = New-Object Windows.Forms.TextBox
$hotkey.Text = $settings.hotkey; $hotkey.Location = New-Object Drawing.Point(20, 302); $hotkey.Width = 450; $form.Controls.Add($hotkey)
Add-Label 'Custom vocabulary (Whisper only)' 342
$vocabulary = New-Object Windows.Forms.TextBox
$vocabulary.Multiline = $true; $vocabulary.ScrollBars = 'Vertical'; $vocabulary.MaxLength = 4000
$vocabulary.Text = $settings.vocabulary; $vocabulary.Location = New-Object Drawing.Point(20, 369); $vocabulary.Size = New-Object Drawing.Size(450, 85)
$form.Controls.Add($vocabulary)
$sounds = New-Object Windows.Forms.CheckBox
$sounds.Text = 'Play recording start / stop sounds'; $sounds.Checked = $settings.sounds
$sounds.Location = New-Object Drawing.Point(20, 470); $sounds.Size = New-Object Drawing.Size(450, 28); $form.Controls.Add($sounds)
Add-Label 'Parakeet detects the language automatically.' 505
$save = New-Object Windows.Forms.Button
$save.Text = 'Save'; $save.Location = New-Object Drawing.Point(270, 542); $save.Size = New-Object Drawing.Size(95, 32)
$cancel = New-Object Windows.Forms.Button
$cancel.Text = 'Cancel'; $cancel.Location = New-Object Drawing.Point(375, 542); $cancel.Size = New-Object Drawing.Size(95, 32)
$cancel.Add_Click({ $form.Close() })
$save.Add_Click({
    $temporary = Join-Path $env:ASP_DATA_DIR ('settings-' + [Guid]::NewGuid() + '.json')
    try {
        $updated = @{ enabled = $enabled.Checked; model = $modelIds[$model.SelectedIndex]; language = $language.Text.Trim(); hotkey = $hotkey.Text.Trim(); vocabulary = $vocabulary.Text; sounds = $sounds.Checked }
        [IO.File]::WriteAllText($temporary, ($updated | ConvertTo-Json), (New-Object Text.UTF8Encoding($false)))
        $output = & $env:ASP_BINARY --data-dir $env:ASP_DATA_DIR dictation configure --settings-file $temporary 2>&1
        if ($LASTEXITCODE -ne 0) { throw ($output -join "`n") }
        $form.Close()
    } catch { [Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Could not save dictation settings', 'OK', 'Error') | Out-Null }
    finally { Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue }
})
$form.Controls.AddRange(@($save, $cancel)); $form.AcceptButton = $save; $form.CancelButton = $cancel
[void]$form.ShowDialog()
