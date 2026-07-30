return function(subject)
    local api = subject(7, {
        seed = function()
            return 1, 2, 3
        end,
        transforms = {
            double = function(value)
                return value * 2
            end,
        },
    })

    local addedOk, added = api.invoke("add", 5)
    local doubledOk, doubled = api.invoke("double", 4)
    local missingOk, missingReason, missingName = api.invoke("absent")

    return addedOk,
        added.label,
        added.revision,
        added.total,
        doubledOk,
        doubled.label,
        doubled.revision,
        doubled.total,
        missingOk,
        missingReason,
        missingName,
        api.state.total,
        api.state.revision,
        api.state.values
end
