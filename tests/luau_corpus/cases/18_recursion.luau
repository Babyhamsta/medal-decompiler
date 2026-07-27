local function factorial(value: number): number
    if value <= 1 then
        return 1
    end
    return value * factorial(value - 1)
end

local isEven

local function isOdd(value: number): boolean
    if value == 0 then
        return false
    end
    return isEven(value - 1)
end

function isEven(value: number): boolean
    if value == 0 then
        return true
    end
    return isOdd(value - 1)
end

return factorial, isEven, isOdd
