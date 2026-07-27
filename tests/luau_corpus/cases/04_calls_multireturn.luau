local function divideWithRemainder(value: number, divisor: number)
    return math.floor(value / divisor), value % divisor
end

local function combine(prefix: string, quotient: number, remainder: number)
    return prefix .. tostring(quotient), remainder * 2
end

local function run(value: number)
    local quotient, remainder = divideWithRemainder(value, 7)
    local label, doubled = combine("q=", divideWithRemainder(value + remainder, 5))
    return combine(label .. ":", quotient + doubled, remainder)
end

return run
