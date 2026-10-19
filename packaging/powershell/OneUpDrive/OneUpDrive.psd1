@{
    RootModule           = 'OneUpDrive.psm1'
    ModuleVersion        = '0.1.0'
    GUID                 = '6b3c2a9e-4f1d-4c87-9a55-2e8d1f0b7c41'
    Author               = 'OneUpDrive contributors'
    Copyright            = 'MIT License'
    Description          = "Control OneUpDrive's background agent (oneupd) from PowerShell: status, pause, resume, wait for sync."
    PowerShellVersion    = '5.1'
    CompatiblePSEditions = @('Desktop', 'Core')
    FunctionsToExport    = @('Get-OneUpStatus', 'Suspend-OneUp', 'Resume-OneUp', 'Wait-OneUpSync')
    CmdletsToExport      = @()
    VariablesToExport    = @()
    AliasesToExport      = @()
}
