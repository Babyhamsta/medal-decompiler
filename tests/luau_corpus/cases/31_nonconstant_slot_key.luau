local function execute(input, index)
    local registers = {}
    registers[1] = input
    registers[index] = input + 7
    registers[2] = registers[1] + 1
    return registers[1], registers[2], registers[3]
end

return execute
