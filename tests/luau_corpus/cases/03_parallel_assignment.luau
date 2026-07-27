local function rotate(first, second, third)
    first, second = second, first
    first, second, third = third, first, second

    local keep, discard, tail = first, second, third
    discard = nil
    keep, tail = tail, keep

    return keep, discard, tail, first, second, third
end

return rotate
