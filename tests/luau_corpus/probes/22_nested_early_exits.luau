return function(subject)
    local groups = {
        { items = { "a", "b" } },
        { disabled = true, items = { "target" } },
        { items = { "c", "target", "d" } },
    }
    return subject(groups, "target"), subject(groups, "absent")
end
