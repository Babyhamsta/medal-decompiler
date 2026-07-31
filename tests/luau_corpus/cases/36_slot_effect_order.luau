-- The folded value keeps its place in the evaluation order. The write's value
-- is a call, and the statement that reads the slot runs another call first,
-- so moving the value into the read swaps the two. Both calls record
-- themselves, making the order observable.
--
-- Every other guard passes here: the read is the very next statement, so
-- nothing intervenes, the slot has one read, and the table never escapes.

local order = {}
local effects = {}

function effects.produce()
    order[#order + 1] = "p"
    return 10
end

function effects.observe()
    order[#order + 1] = "o"
    return 1
end

local function execute(input)
    order = {}

    local registers = {}
    local label = tostring(input)
    registers[3] = effects.produce()
    local total = effects.observe() + registers[3] + input

    return total, table.concat(order, ""), label
end

return execute
