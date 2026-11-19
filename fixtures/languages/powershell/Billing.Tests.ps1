BeforeAll {
    . $PSScriptRoot/Billing.psm1
}

Describe 'Get-InvoiceTotal' {
    It 'totals items' {
        Get-InvoiceTotal -Items 1, 2 | Should -Be 6
    }
}
