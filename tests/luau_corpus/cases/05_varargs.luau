local function capture(...)
    local count = select("#", ...)
    local first, second, third = ...
    local tail = { select(2, ...) }
    return count, first, second, third, tail
end

local function forward(prefix: string, ...)
    local count, first, second, third, tail = capture(...)
    return prefix, count, first, second, third, tail, capture(...)
end

return forward
