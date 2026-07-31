return function(subject)
    local groups = {
        { items = { "a", "b" } },
        { disabled = true, items = { "target" } },
        { items = { "c", false, "target" } },
        { items = { "target", "d" } },
    }

    local outerFound, innerFound, itemFound = subject(groups, "target")
    local missing = subject(groups, "absent")

    return outerFound, innerFound, itemFound, missing
end
