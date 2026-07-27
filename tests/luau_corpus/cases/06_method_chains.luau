local Box = {}
Box.__index = Box

function Box.new(value: number)
    return setmetatable({ value = value }, Box)
end

function Box:add(delta: number)
    self.value += delta
    return self
end

function Box:scale(factor: number)
    self.value *= factor
    return self
end

function Box:read()
    return self.value
end

local function run(seed: number, options)
    local key = options.key
    local offset = options[key]
    return Box.new(seed):add(offset):scale(options.factor):read()
end

return run
