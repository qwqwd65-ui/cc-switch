[CmdletBinding()]
param([Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Paths)

$ErrorActionPreference = 'Stop'
foreach ($path in $Paths) {
    $tokens = $null
    $parseErrors = $null
    [Management.Automation.Language.Parser]::ParseFile(
        (Resolve-Path -LiteralPath $path).Path, [ref]$tokens, [ref]$parseErrors
    ) | Out-Null
    if ($parseErrors.Count -gt 0) {
        foreach ($parseError in $parseErrors) {
            Write-Host "${path}:$($parseError.Extent.StartLineNumber): $($parseError.Message)"
        }
        throw "PowerShell syntax check failed: $path"
    }
    Write-Host "PowerShell syntax valid: $path"
}
