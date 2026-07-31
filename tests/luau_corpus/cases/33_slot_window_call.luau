-- Precondition 4: a call between a slot write and its read ends the fold
-- window. The written value is a plain upvalue read, so the effectful-value
-- guard does not apply, and the call changes that upvalue without the
-- statement itself writing it, so the source-overwrite guard does not either.
-- Folding across the call is the only way to get a different answer.
--
-- `helpers.bump` is a table field rather than a local function so the Luau
-- compiler cannot inline it into the window, and `state = input` sits between
-- the table literal and the slot write so constructor folding cannot absorb
-- the write.

local state = 0
local helpers = {}

function helpers.bump()
    state = state + 100
    return 7
end

local function execute(input)
    local registers = {}
    state = input
    registers[3] = state
    local carried = helpers.bump()
    registers[4] = registers[3] * 1000 + state + carried

    return registers[4], state
end

return execute
