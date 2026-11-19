using Test
include("Billing.jl")
using .Billing

@testset "totals items" begin
    invoice = Billing.Invoice("a", [1, 2])
    @test Billing.total(invoice) == 6
end
