local function run(events)
    local state = "idle"
    local index = 0
    local score = 0

    while true do
        index += 1
        local event = events[index]

        if event == nil then
            state = "done"
        elseif state == "idle" then
            if event == "start" then
                state = "running"
            else
                continue
            end
        elseif state == "running" then
            if event == "pause" then
                state = "paused"
            elseif event == "stop" then
                state = "done"
            else
                score += 1
            end
        elseif state == "paused" then
            if event == "resume" then
                state = "running"
            elseif event == "stop" then
                state = "done"
            end
        end

        if state == "done" then
            break
        end
    end

    return state, score, index
end

return run
