local dynamicKey = "score"
local nested = {
    10,
    20,
    name = "alpha",
    ["not an identifier"] = true,
    [dynamicKey] = 99,
    child = {
        x = 4,
        y = 8,
        { "deep", false },
    },
}

return {
    array = { 1, 2, 3, 5, 8 },
    record = { left = "L", right = "R" },
    mixed = nested,
    [nested.name] = nested[dynamicKey],
}
