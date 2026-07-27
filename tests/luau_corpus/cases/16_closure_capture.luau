local function makeTransform(offset: number, factor: number)
    local immutableLabel = "transform"

    local function scale(value: number)
        return value * factor
    end

    return function(value: number)
        local function addOffset()
            return scale(value) + offset
        end

        return immutableLabel, addOffset()
    end
end

return makeTransform
