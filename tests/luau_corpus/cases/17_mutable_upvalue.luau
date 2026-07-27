local function makeCounter(seed: number)
    local value = seed

    local function increment(amount: number?)
        value += amount or 1
        return value
    end

    local function decrement(amount: number?)
        value -= amount or 1
        return value
    end

    local function read()
        return value
    end

    return increment, decrement, read
end

return makeCounter
