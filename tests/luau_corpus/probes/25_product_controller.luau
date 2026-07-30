return function(subject)
    local controller = subject.new({
        fallback = function(context)
            return "fallback:" .. context.action
        end,
    }, { retries = 5 })

    controller:use(function(context, index)
        context.seen = index
        return context
    end)

    controller:on("ping", function(context)
        return "pong:" .. tostring(context.seen)
    end)

    local okPing, pingValue = controller:dispatch("ping", 1)
    local okMissing, missingValue = controller:dispatch("missing", 2)

    return okPing, pingValue, okMissing, missingValue, #controller.history
end
