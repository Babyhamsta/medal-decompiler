local nothing = nil
local enabled = true
local count = 12
local ratio = 3.5
local message = "medal"

do
    local shadow = count + 2
    count = shadow * 3
end

enabled = not enabled

return {
    nothing = nothing,
    enabled = enabled,
    count = count,
    ratio = ratio,
    message = message,
}
