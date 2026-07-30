local function execute(input)
    local registers = {}
    local alias = registers
    registers[1] = input
    alias[1] = input + 50
    registers[2] = registers[1] * 2
    return registers[1], registers[2], alias[2]
end

return execute
