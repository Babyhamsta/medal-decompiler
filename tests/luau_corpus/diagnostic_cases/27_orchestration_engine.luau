local function pack(...)
    return {
        n = select("#", ...),
        ...,
    }
end

local function appendPacked(target, ...)
    local values = pack(...)

    for index = 1, values.n do
        target.n += 1
        target[target.n] = values[index]
    end

    return target
end

local function cloneArray(values)
    local result = table.create(#values)
    table.move(values, 1, #values, 1, result)
    return result
end

local function sortedKeys(source)
    local keys = {}

    for key in pairs(source) do
        keys[#keys + 1] = key
    end

    table.sort(keys, function(left, right)
        local leftType = type(left)
        local rightType = type(right)

        if leftType == rightType then
            return left < right
        end

        return leftType < rightType
    end)

    return keys
end

local function checksum(value, seen)
    local valueType = type(value)

    if valueType == "nil" then
        return 11
    elseif valueType == "boolean" then
        return if value then 23 else 29
    elseif valueType == "number" then
        return math.floor(math.abs(value) * 100 + 0.5) % 65521
    elseif valueType == "string" then
        local result = 0

        for index = 1, #value do
            result = (result * 33 + string.byte(value, index)) % 65521
        end

        return result
    elseif valueType ~= "table" then
        return checksum(valueType, seen)
    end

    seen = seen or {}

    if seen[value] then
        return 37
    end

    seen[value] = true

    local result = 41

    for _, key in ipairs(sortedKeys(value)) do
        result = (result * 131 + checksum(key, seen)) % 65521
        result = (result * 131 + checksum(value[key], seen)) % 65521
    end

    seen[value] = nil
    return result
end

local Accumulator = {}
Accumulator.__index = Accumulator

function Accumulator.new(seed)
    return setmetatable({
        total = seed,
        count = 0,
        values = {},
    }, Accumulator)
end

function Accumulator:add(value)
    self.count += 1
    self.total += value
    self.values[self.count] = value
    return self.total, self.count
end

function Accumulator:summary(label)
    return {
        label = label,
        total = self.total,
        count = self.count,
        values = cloneArray(self.values),
    }
end

Accumulator.__call = function(self, value)
    return self:add(value)
end

Accumulator.__len = function(self)
    return self.count
end

Accumulator.__tostring = function(self)
    return string.format("Accumulator<%d:%d>", self.count, self.total)
end

local Engine = {}
Engine.__index = Engine

function Engine.new(seed, options)
    local copiedOptions = table.clone(options or {})

    return setmetatable({
        seed = seed,
        options = table.freeze(copiedOptions),
        tasks = {},
        middleware = {},
        listeners = {},
        history = {},
        retryCounts = {},
        results = {},
        sequence = 0,
        running = false,
    }, Engine)
end

function Engine:_record(kind, label, payload)
    self.sequence += 1

    local entry = {
        sequence = self.sequence,
        kind = kind,
        label = label,
        payload = payload,
    }

    table.insert(self.history, entry)
    return entry, self.sequence
end

function Engine:use(callback)
    local slot = #self.middleware + 1
    self.middleware[slot] = callback
    local active = true

    return function()
        if active and self.middleware[slot] == callback then
            active = false
            self.middleware[slot] = false
            return true, slot
        end

        return false, slot
    end
end

function Engine:on(eventName, callback)
    local listeners = self.listeners[eventName]

    if listeners == nil then
        listeners = {}
        self.listeners[eventName] = listeners
    end

    local slot = #listeners + 1
    listeners[slot] = callback
    local active = true

    return function()
        if active and listeners[slot] == callback then
            active = false
            listeners[slot] = false
            return true, eventName, slot
        end

        return false, eventName, slot
    end
end

function Engine:emit(eventName, ...)
    local listeners = self.listeners[eventName] or {}
    local returned = {
        n = 0,
    }
    local delivered = 0

    for _, callback in ipairs(listeners) do
        if callback == false then
            continue
        end

        delivered += 1
        appendPacked(returned, callback(...))
    end

    self:_record("event", eventName, {
        delivered = delivered,
        returned = returned,
    })

    return delivered, table.unpack(returned, 1, returned.n)
end

function Engine:register(name, dependencies, handler, retries, fallback)
    assert(self.tasks[name] == nil, "duplicate task: " .. name)

    local task = {
        name = name,
        dependencies = cloneArray(dependencies or {}),
        handler = handler,
        retries = retries or 0,
        fallback = fallback,
    }

    self.tasks[name] = task
    return task
end

function Engine:replace(name, callback)
    local task = assert(self.tasks[name], "missing task: " .. name)
    local previous = task.handler
    local active = true
    task.handler = callback

    return previous, function()
        if active and task.handler == callback then
            active = false
            task.handler = previous
            return true, previous
        end

        return false, task.handler
    end
end

function Engine:_resolve(name, visiting, visited, order)
    if visited[name] then
        return
    end

    if visiting[name] then
        error("dependency cycle at " .. name)
    end

    local task = assert(self.tasks[name], "missing dependency: " .. name)
    visiting[name] = true

    for _, dependency in ipairs(task.dependencies) do
        self:_resolve(dependency, visiting, visited, order)
    end

    visiting[name] = nil
    visited[name] = true
    order[#order + 1] = name
end

function Engine:_applyMiddleware(context, ...)
    local metadata = {}

    for index, callback in ipairs(self.middleware) do
        if callback == false then
            continue
        end

        local values = pack(callback(context, index, ...))
        local nextContext = values[1]

        if nextContext == nil then
            return nil, values[2] or ("middleware " .. index .. " rejected"), metadata
        end

        context = nextContext
        metadata[#metadata + 1] = values[2]
    end

    return context, metadata, select("#", ...)
end

function Engine:_executeTask(task, context, ...)
    local extra = pack(...)
    local attempt = 0
    local lastError

    repeat
        attempt += 1

        local called = pack(pcall(
            task.handler,
            context,
            table.unpack(extra, 1, extra.n)
        ))

        if called[1] then
            self.retryCounts[task.name] = attempt - 1
            self:_record("task", task.name, {
                attempt = attempt,
                ok = true,
                width = called.n - 1,
            })

            return true, table.unpack(called, 2, called.n)
        end

        lastError = called[2]
        self:_record("retry", task.name, {
            attempt = attempt,
            error = tostring(lastError),
        })
    until attempt > task.retries

    self.retryCounts[task.name] = attempt - 1

    if task.fallback ~= nil then
        local recovered = pack(task.fallback(
            context,
            lastError,
            table.unpack(extra, 1, extra.n)
        ))

        self:_record("fallback", task.name, {
            width = recovered.n,
        })

        return true, table.unpack(recovered, 1, recovered.n)
    end

    return false, lastError
end

function Engine:historyIterator(kind)
    local index = 0

    return function()
        while index < #self.history do
            index += 1

            local entry = self.history[index]

            if kind == nil or entry.kind == kind then
                return index, entry
            end
        end
    end
end

function Engine:run(name, payload, ...)
    if self.running then
        return false, "engine already running"
    end

    self.running = true

    local extra = pack(...)
    local context = {
        seed = self.seed,
        payload = table.clone(payload or {}),
        results = {},
        trace = {},
        flags = {},
    }
    local prepared, middlewareMetadata, extraCount = self:_applyMiddleware(
        context,
        table.unpack(extra, 1, extra.n)
    )

    if prepared == nil then
        self.running = false
        return false, middlewareMetadata
    end

    context = prepared
    context.middleware = middlewareMetadata
    context.extraCount = extraCount

    local order = {}
    local resolved = pack(pcall(function()
        self:_resolve(name, {}, {}, order)
    end))

    if not resolved[1] then
        self.running = false
        return false, resolved[2]
    end

    local finalValues = {
        n = 0,
    }

    for _, taskName in ipairs(order) do
        local task = self.tasks[taskName]
        local values = pack(self:_executeTask(
            task,
            context,
            table.unpack(extra, 1, extra.n)
        ))

        if not values[1] then
            self.running = false
            self:emit("failed", taskName, values[2])
            return false, taskName, values[2]
        end

        local taskValues = {
            n = values.n - 1,
        }

        for index = 2, values.n do
            taskValues[index - 1] = values[index]
        end

        context.results[taskName] = taskValues
        finalValues = taskValues
    end

    self.results = context.results
    self.running = false

    local delivered, firstListenerValue = self:emit(
        "completed",
        name,
        context,
        table.unpack(finalValues, 1, finalValues.n)
    )

    return true,
        context,
        delivered,
        firstListenerValue,
        table.unpack(finalValues, 1, finalValues.n)
end

local function touch(context, label, value)
    context.trace[#context.trace + 1] = label
    return value
end

local function createTasks(engine)
    local unstableAttempts = 0
    local accumulator = Accumulator.new(engine.seed)

    local definitions = {
        {
            name = "prepare",
            dependencies = {},
            retries = 0,
            handler = function(context, ...)
                local source = context.payload.values or {}
                local values = table.create(#source)

                for index, value in ipairs(source) do
                    values[index] = math.clamp(value + context.seed, -1000, 1000)
                end

                table.sort(values)

                local title = context.payload.title or "untitled"
                title = string.lower(string.gsub(title, "^%s*(.-)%s*$", "%1"))

                return {
                    title = title,
                    values = values,
                    checksum = checksum(values),
                    extraCount = select("#", ...),
                }, #values
            end,
        },
        {
            name = "enrich",
            dependencies = {
                "prepare",
            },
            retries = 1,
            handler = function(context, ...)
                unstableAttempts += 1

                if unstableAttempts % 2 == 1 then
                    error("transient enrichment failure")
                end

                local prepared = context.results.prepare[1]
                local aliases = {
                    prepared.values,
                    prepared.values,
                }
                local total = 0

                for index, value in ipairs(aliases[1]) do
                    aliases[2][index] = value + index
                    total += aliases[1][index]
                end

                local mode = if total % 2 == 0 then "even" else "odd"
                return total, mode, prepared, select("#", ...)
            end,
            fallback = function(context, reason)
                local prepared = context.results.prepare[1]
                return 0, "fallback", prepared, tostring(reason)
            end,
        },
        {
            name = "aggregate",
            dependencies = {
                "prepare",
                "enrich",
            },
            retries = 0,
            handler = function(context)
                local prepared = context.results.prepare[1]
                local enrichedTotal = context.results.enrich[1]

                for _, value in ipairs(prepared.values) do
                    accumulator(value)
                end

                local byKey = {
                    prepared = checksum(prepared),
                    enriched = enrichedTotal,
                    trace = checksum(context.trace),
                }
                local ordered = {}

                for _, key in ipairs(sortedKeys(byKey)) do
                    ordered[#ordered + 1] = string.format("%s=%s", key, tostring(byKey[key]))
                end

                return accumulator:summary(table.concat(ordered, "|")),
                    #accumulator,
                    tostring(accumulator)
            end,
        },
        {
            name = "finalize",
            dependencies = {
                "aggregate",
            },
            retries = 0,
            handler = function(context, ...)
                local summary = context.results.aggregate[1]
                local accumulatorCount = context.results.aggregate[2]
                local description = context.results.aggregate[3]
                local token = bit32.bxor(
                    checksum(summary),
                    checksum(context.payload),
                    context.seed
                )

                return {
                    status = "complete",
                    title = context.results.prepare[1].title,
                    total = summary.total,
                    count = accumulatorCount,
                    description = description,
                    token = token,
                    dynamic = {
                        [context.payload.dynamicKey or "default"] = summary.count,
                    },
                }, token, select("#", ...)
            end,
        },
    }

    for index, definition in ipairs(definitions) do
        local capturedIndex = index
        local capturedName = definition.name
        local capturedHandler = definition.handler

        engine:register(
            capturedName,
            definition.dependencies,
            function(context, ...)
                context.trace[#context.trace + 1] = capturedName .. ":" .. capturedIndex
                return capturedHandler(context, ...)
            end,
            definition.retries,
            definition.fallback
        )
    end

    return accumulator
end

local function defaultCommands()
    return {
        {
            action = "run",
            payload = {
                title = "  Alpha Job  ",
                values = {
                    4,
                    1,
                    3,
                },
                dynamicKey = "first",
            },
        },
        {
            action = "replace",
        },
        {
            action = "run",
            payload = {
                title = "  Beta Job  ",
                values = {
                    2,
                    5,
                },
                dynamicKey = "second",
            },
        },
        {
            action = "restore",
        },
        {
            action = "unsubscribe",
        },
        {
            action = "run",
            payload = {
                title = "  Gamma Job  ",
                values = {
                    8,
                    -2,
                    6,
                },
                dynamicKey = "third",
            },
        },
    }
end

local function runScenario(seed, commands, ...)
    local engine = Engine.new(seed or 7, {
        mode = "diagnostic",
        retries = 1,
    })
    local forwarded = pack(...)
    local accumulator = createTasks(engine)
    local listenerLog = {}
    local runResults = {}
    local restoreFinalizer

    engine:use(function(context, index)
        context.flags.valid = type(context.payload) == "table"
        return context, "validate:" .. index
    end)

    engine:use(function(context, index, ...)
        local shouldTrace = touch(context, "middleware:" .. index, true)
        context.flags.forwarded = shouldTrace and select("#", ...) or 0
        return context, "trace:" .. index
    end)

    engine:use(function(context, index)
        local values = context.payload.values
        local accepted = values ~= nil and #values > 0

        if not accepted then
            return nil, "empty values at middleware " .. index
        end

        context.alias = values
        return context, "values:" .. #values
    end)

    local unsubscribeCompleted = engine:on("completed", function(name, context, ...)
        local width = select("#", ...)
        listenerLog[#listenerLog + 1] = string.format(
            "%s:%s:%d",
            name,
            context.results.prepare[1].title,
            width
        )
        return name, width
    end)

    engine:on("completed", function(name)
        listenerLog[#listenerLog + 1] = "secondary:" .. name
        return "secondary"
    end)

    engine:on("failed", function(name, reason)
        listenerLog[#listenerLog + 1] = "failed:" .. name
        return tostring(reason)
    end)

    for commandIndex, command in ipairs(commands or defaultCommands()) do
        if command.action == "replace" then
            local previous
            previous, restoreFinalizer = engine:replace("finalize", function(context, ...)
                local values = pack(previous(context, ...))
                values[1].status = "replaced"
                values[1].replacement = commandIndex
                return table.unpack(values, 1, values.n)
            end)
            continue
        elseif command.action == "restore" then
            if restoreFinalizer ~= nil then
                local restored = restoreFinalizer()
                engine:_record("control", "restore", restored)
            end
            continue
        elseif command.action == "unsubscribe" then
            local removed, eventName, slot = unsubscribeCompleted()
            engine:_record("control", eventName, {
                removed = removed,
                slot = slot,
            })
            continue
        elseif command.action == "stop" then
            engine:_record("control", "stop", commandIndex)
            break
        elseif command.action ~= "run" then
            engine:_record("control", "unknown", command.action)
            continue
        end

        local result = pack(engine:run(
            "finalize",
            command.payload,
            commandIndex,
            table.unpack(forwarded, 1, forwarded.n)
        ))

        runResults[#runResults + 1] = result

        if not result[1] then
            break
        end
    end

    local retries = {}

    for key, value in pairs(engine.retryCounts) do
        retries[key] = value
    end

    local taskHistory = {}

    for _, entry in engine:historyIterator("task") do
        taskHistory[#taskHistory + 1] = entry.label .. ":" .. entry.payload.attempt
    end

    local report = {
        status = runResults[#runResults] and (if runResults[#runResults][1] then "ok" else "failed")
            or "empty",
        runs = runResults,
        listeners = listenerLog,
        taskHistory = taskHistory,
        retries = retries,
        historyCount = #engine.history,
        sequence = engine.sequence,
        accumulator = accumulator:summary("final"),
        checksum = checksum({
            results = runResults,
            listeners = listenerLog,
            tasks = taskHistory,
            retries = retries,
        }),
    }
    local tail = pack(
        "diagnostic",
        nil,
        report.sequence,
        report.accumulator.total
    )

    return report.status,
        report.checksum,
        report,
        table.unpack(tail, 1, tail.n)
end

return runScenario
