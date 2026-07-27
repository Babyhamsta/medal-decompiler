local function countDown(_, state)
    local nextValue = state - 1
    if nextValue >= 0 then
        return nextValue, nextValue * nextValue
    end
end

local function collect(values)
    local result = {}

    for index, value in ipairs(values) do
        result[index] = value * 2
    end

    for key, value in pairs(values) do
        if type(key) == "string" then
            result[key] = value
        end
    end

    for index, square in countDown, nil, 5 do
        result[#result + 1] = index + square
    end

    return result
end

return collect
