local function finiteNumber(value)
    return value == value and value ~= math.huge and value ~= -math.huge
end

local function packedWidth(value)
    local width = rawget(value, "n")

    if type(width) ~= "number"
        or not finiteNumber(width)
        or width < 0
        or width % 1 ~= 0
    then
        return nil
    end

    for key in pairs(value) do
        if key ~= "n"
            and (
                type(key) ~= "number"
                or key % 1 ~= 0
                or key < 1
                or key > width
            )
        then
            return nil
        end
    end

    return width
end

local function encode(value, active)
    local valueType = type(value)

    if valueType == "nil" then
        return "n;"
    elseif valueType == "boolean" then
        return if value then "b1;" else "b0;"
    elseif valueType == "number" then
        if not finiteNumber(value) then
            error("non-finite numbers are unsupported")
        end

        return "d" .. string.format("%.17g", value) .. ";"
    elseif valueType == "string" then
        return "s" .. #value .. ":" .. value .. ";"
    elseif valueType ~= "table" then
        error("unsupported semantic result type: " .. valueType)
    end

    if active[value] then
        error("cyclic semantic result tables are unsupported")
    end

    active[value] = true

    local width = packedWidth(value)

    if width ~= nil then
        local parts = table.create(width + 2)
        parts[1] = "p" .. width .. "["

        for index = 1, width do
            parts[index + 1] = encode(value[index], active)
        end

        parts[width + 2] = "]"
        active[value] = nil
        return table.concat(parts)
    end

    local entries = {}

    for key, item in pairs(value) do
        local keyType = type(key)

        if keyType ~= "string" and keyType ~= "number" and keyType ~= "boolean" then
            error("unsupported semantic table key type: " .. keyType)
        end

        entries[#entries + 1] = {
            key = encode(key, active),
            value = encode(item, active),
        }
    end

    table.sort(entries, function(left, right)
        return left.key < right.key
    end)

    local parts = table.create(#entries + 2)
    parts[1] = "t" .. #entries .. "{"

    for index, entry in ipairs(entries) do
        parts[index + 1] = entry.key .. entry.value
    end

    parts[#entries + 2] = "}"
    active[value] = nil
    return table.concat(parts)
end

local subjectPath, probePath = ...
assert(type(subjectPath) == "string", "expected subject module path")
assert(type(probePath) == "string", "expected probe module path")

local subject = require(subjectPath)
local probe = require(probePath)
local values = table.pack(probe(subject))

print("SEMANTIC_RESULT " .. encode(values, {}))
