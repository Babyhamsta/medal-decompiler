local function execute(input)
    local backing = {}
    local registers = {}

    setmetatable(registers, {
        __index = function(_, key)
            return (backing[key] or 0) + 1000
        end,
        __newindex = function(_, key, value)
            backing[key] = value * 2
        end,
    })

    registers[1] = input
    registers[2] = registers[1] + 1

    return registers[1], registers[2], backing[1], backing[2]
end

return execute
