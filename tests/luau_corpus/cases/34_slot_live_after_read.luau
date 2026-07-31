-- Precondition 7: the slot is read again after the fold's read, so the write
-- cannot be removed. Nothing else stops this fold — the read is the very next
-- statement, the table never escapes, and the value crosses nothing — so the
-- liveness check is the only thing keeping it correct.
--
-- The two later reads are kept in separate statements, and a `tostring` call
-- sits between the table literal and the slot write, so neither constructor
-- folding nor the one-read-per-window check gets there first.

local function execute(input)
    local registers = {}
    local label = tostring(input)
    registers[3] = input + 1
    registers[4] = registers[3] * 2
    local echo = tostring(registers[3])

    return registers[4], echo, label
end

return execute
