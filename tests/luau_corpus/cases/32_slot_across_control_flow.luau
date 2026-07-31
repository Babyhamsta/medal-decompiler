local function execute(input, flag)
    local registers = {}
    registers[1] = input

    if flag then
        registers[2] = input * 3
    else
        registers[2] = input - 3
    end

    registers[3] = registers[2] + registers[1]
    return registers[1], registers[2], registers[3]
end

return execute
