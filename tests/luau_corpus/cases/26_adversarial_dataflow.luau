local function pack(...)
    return select("#", ...), { ... }
end

local function build(seed, source)
    local state = {
        total = seed,
        revision = 0,
        values = {},
        callbacks = {},
    }
    local seeded = { source.seed() }
    seeded.status = "ready"
    state.seeded = seeded

    local registry = {}
    registry.current = function()
        return registry
    end
    state.registry = registry

    local step
    local function snapshot(label)
        local revision = state.revision
        local total = state.total
        return {
            label = label,
            revision = revision,
            total = total,
            resume = function(value)
                return step(value, revision)
            end,
        }
    end

    step = function(value, expectedRevision)
        if expectedRevision ~= nil and expectedRevision ~= state.revision then
            return false, snapshot("stale")
        end

        state.revision += 1
        state.total += value
        state.values[state.revision] = value
        return true, snapshot("accepted")
    end

    local routes = {}
    routes.add = step
    routes.snapshot = snapshot
    routes.batch = function(...)
        local count, values = pack(...)
        local results = {}

        for index = 1, count do
            local ok, result = step(values[index])
            results[index] = {
                ok = ok,
                result = result,
            }
        end

        return count, table.unpack(results)
    end

    for key, transform in pairs(source.transforms) do
        local capturedKey = key
        local capturedTransform = transform
        state.callbacks[key] = function(value, ...)
            local before = snapshot("before:" .. capturedKey)
            local first, second = capturedTransform(value, ...)
            local ok, after = step(first or 0, before.revision)

            return ok, after, {
                key = capturedKey,
                before = before,
                extra = second,
            }
        end
    end

    local api = {
        state = state,
        routes = routes,
    }
    api.invoke = function(name, ...)
        local callback = state.callbacks[name] or routes[name]
        if callback == nil then
            return false, "missing", name
        end

        return callback(...)
    end
    api.replace = function(name, callback)
        local previous = state.callbacks[name]
        state.callbacks[name] = callback

        return previous, function()
            state.callbacks[name] = previous
        end
    end

    return api, snapshot("ready")
end

return build
