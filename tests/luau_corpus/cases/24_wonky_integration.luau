local Machine = {}
Machine.__index = Machine

function Machine.new(seed: number)
    return setmetatable({
        state = "cold",
        total = seed,
        history = {},
    }, Machine)
end

function Machine:record(label: string, value)
    self.history[#self.history + 1] = {
        label = label,
        value = value,
        state = self.state,
    }
    return value
end

function Machine:transition(command: string, ...)
    local arguments = { ... }
    local first = arguments[1]

    if self.state == "cold" then
        self.state = if command == "start" then "running" else "cold"
        return self:record(command, first or 0)
    elseif self.state == "running" then
        if command == "add" then
            self.total += first or 0
        elseif command == "pause" then
            self.state = "paused"
        elseif command == "stop" then
            self.state = "done"
        else
            return self:record("unknown", command)
        end
    elseif self.state == "paused" then
        self.state = if command == "resume" then "running" else self.state
    end

    return self:record(command, self.total), self.state
end

local function attempt(callback, ...)
    local values = { callback(...) }
    return values[1] ~= nil, table.unpack(values)
end

local function build(seed: number, commands)
    local machine = Machine.new(seed)
    local callbacks = {}

    for index, entry in ipairs(commands) do
        callbacks[index] = function(extra)
            local command = entry.command
            local value = entry.value or extra
            return machine:transition(command, value)
        end
    end

    local index = 0
    while machine.state ~= "done" and index < #callbacks do
        index += 1
        local ok, value, state = attempt(callbacks[index], index)

        if not ok then
            machine:record("error", index)
            continue
        end

        if state == "paused" and commands[index].autoResume then
            machine:transition("resume")
        elseif value == false then
            break
        end
    end

    return {
        state = machine.state,
        total = machine.total,
        history = machine.history,
        callbacks = callbacks,
    }
end

return build
