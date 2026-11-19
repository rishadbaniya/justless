using namespace System.Collections.Generic
Import-Module "$PSScriptRoot/Ledger.psm1"

class InvoiceService {
    [string] $Currency = 'EUR'

    [int] Total([int[]] $Items) {
        $sum = ($Items | Measure-Object -Sum).Sum
        return Invoke-ApplyTax -Amount $sum
    }
}

function Invoke-ApplyTax {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [int] $Amount
    )
    return $Amount * 2
}

function Get-InvoiceTotal([int[]] $Items) {
    $service = [InvoiceService]::new()
    $service.Total($Items)
}

Export-ModuleMember -Function Invoke-ApplyTax, Get-InvoiceTotal
