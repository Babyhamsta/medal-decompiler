local function observe(slots)
    slots[2] = slots[2] + 100
    return slots
end

local function execute(input)
    local registers = {}
    registers[1] = input
    registers[2] = input * 2
    observe(registers)
    registers[3] = registers[2] + 1
    return registers[2], registers[3]
end

return execute
