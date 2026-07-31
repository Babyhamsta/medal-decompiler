-- Precondition 7's escape check: the table is handed to a call after the
-- fold's read, so the write is not dead even though the slot has exactly one
-- read inside this function. The callee reads the slot through the table it
-- was given, which no per-slot read count can see.
--
-- `sink.peek` is a table field so the Luau compiler cannot inline it, and a
-- `tostring` call sits between the table literal and the slot write so
-- constructor folding cannot absorb the write.

local sink = {}

function sink.peek(slots)
    return slots[3]
end

local function execute(input)
    local registers = {}
    local label = tostring(input)
    registers[3] = input + 1
    registers[4] = registers[3] * 2
    local seen = sink.peek(registers)

    return registers[4], seen, label
end

return execute
