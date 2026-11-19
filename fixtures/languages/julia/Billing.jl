module Billing

using Dates
import Base: show

export Invoice, total, apply_tax

abstract type AbstractLedger end

struct Invoice
    id::String
    items::Vector{Int}
end

const CURRENCY = "EUR"

apply_tax(amount::Integer) = amount * 2

function total(invoice::Invoice)
    sum = 0
    for item in invoice.items
        sum += item
    end
    return apply_tax(sum)
end

macro cents(value)
    return :($(esc(value)) * 100)
end

Base.show(io::IO, invoice::Invoice) = print(io, "Invoice ", invoice.id)

end # module
