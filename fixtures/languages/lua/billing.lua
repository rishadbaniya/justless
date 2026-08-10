local M = {}

local function apply_tax(amount)
  return amount * 2
end

function M.total(items)
  local sum = 0
  for _, value in ipairs(items) do
    sum = sum + value
  end
  return apply_tax(sum)
end

local format_total = function(amount)
  return string.format("%d", amount)
end

function M:describe()
  return format_total(self.total({}))
end

return M
