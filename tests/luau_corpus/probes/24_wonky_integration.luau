return function(subject)
    local built = subject(5, {
        { command = "start", value = 1 },
        { command = "add", value = 4 },
        { command = "pause", autoResume = true },
        { command = "add", value = 6 },
        { command = "stop" },
    })
    return built.state, built.total, built.history
end
