return function(subject)
    local built = subject(5, {
        { command = "start", value = 1 },
        { command = "add", value = 4 },
        { command = "pause", autoResume = true },
        { command = "add", value = 6 },
        { command = "stop" },
    })

    -- Drives the unknown-command branch (a non-nil command outside
    -- add/pause/stop), the not-ok/continue branch (a missing command
    -- makes the handler's first return value nil), and the value == false
    -- break branch (an explicit `false` command makes the handler return
    -- false as its first value).
    local edgeCases = subject(2, {
        { command = "start", value = 1 },
        { command = "weird", value = 2 },
        { value = 3 },
        { command = false, value = 4 },
    })

    return built.state,
        built.total,
        built.history,
        edgeCases.state,
        edgeCases.total,
        edgeCases.history
end
