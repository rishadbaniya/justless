-- Valid Lua that an edit may write: attributes, goto, metatables and long strings.
local json <const> = require("json")

local Stack = {}
Stack.__index = Stack

function Stack.new(...)
  local self = setmetatable({ items = { ... }, size = select("#", ...) }, Stack)
  return self
end

function Stack:push(value)
  self.size = self.size + 1
  self.items[self.size] = value
  return self
end

local function halves(n)
  return n // 2, n % 2, n ~ 1, n << 1
end

for i = 1, 10 do
  if i % 2 == 0 then goto continue end
  print(halves(i))
  ::continue::
end

do
  local handle <close> = setmetatable({}, { __close = function() end })
end

local doc = [==[
  a long string with ]] inside
]==]

return { Stack = Stack, doc = doc, encode = json.encode }
