local Controller = {}
Controller.__index = Controller

local DEFAULT_OPTIONS = {
    retries = 2,
    mode = "strict",
    tags = {},
}

local function mergeOptions(overrides)
    local options = {
        retries = DEFAULT_OPTIONS.retries,
        mode = DEFAULT_OPTIONS.mode,
        tags = {},
    }

    if overrides then
        for key, value in pairs(overrides) do
            options[key] = value
        end
    end

    return options
end

function Controller.new(adapter, overrides)
    local self = setmetatable({
        adapter = adapter,
        options = mergeOptions(overrides),
        handlers = {},
        middleware = {},
        history = {},
        sequence = 0,
    }, Controller)

    self.handlers.health = function(context)
        return true, {
            action = context.action,
            sequence = context.sequence,
            mode = self.options.mode,
        }
    end

    return self
end

function Controller:use(callback)
    self.middleware[#self.middleware + 1] = callback
    return self
end

function Controller:on(action, callback)
    local previous = self.handlers[action]
    self.handlers[action] = callback

    return function()
        if self.handlers[action] == callback then
            self.handlers[action] = previous
            return true
        end

        return false
    end
end

function Controller:_applyMiddleware(context)
    local current = context

    for index, callback in ipairs(self.middleware) do
        local nextValue = callback(current, index)
        if nextValue == nil then
            return nil, index
        end
        current = nextValue
    end

    return current, #self.middleware
end

function Controller:dispatch(action, payload)
    self.sequence += 1

    local context = {
        action = action,
        payload = payload,
        sequence = self.sequence,
        options = self.options,
    }

    local prepared, middlewareCount = self:_applyMiddleware(context)
    if prepared == nil then
        return false, {
            action = action,
            stage = "middleware",
            index = middlewareCount,
        }
    end

    local handler = self.handlers[action]
    if handler == nil then
        handler = self.adapter.fallback
    end

    local ok, value, metadata = pcall(handler, prepared)
    local entry = {
        ok = ok,
        action = action,
        sequence = self.sequence,
        middleware = middlewareCount,
    }
    entry.value = value
    entry.metadata = metadata
    self.history[#self.history + 1] = entry

    return ok, value, {
        metadata = metadata,
        historySize = #self.history,
        unregister = self:on("once:" .. action, function()
            return self.history[#self.history]
        end),
    }
end

return {
    new = Controller.new,
    defaults = DEFAULT_OPTIONS,
    class = Controller,
}
