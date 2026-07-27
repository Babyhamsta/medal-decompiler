local function build(key: string, value)
    local result = {}
    result[1] = "first"
    result[2] = "second"
    result.name = "incremental"
    result[key] = value
    result.child = {}
    result.child.enabled = true
    result.child[1] = value
    result[#result + 1] = "tail"
    return result
end

return build
