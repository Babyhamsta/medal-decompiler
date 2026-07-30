local function execute(input)
    local registers = {}
    registers[1] = input
    registers[2] = math.floor
    registers[3] = registers[1] / 2
    registers[3] = registers[2](registers[3])
    registers[4] = registers[3] + registers[1]
    return registers[3], registers[4]
end

return execute
