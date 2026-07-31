-- More live locals than Luau's 200-register ceiling once the decompiler
-- emits one per SSA value, so the output has to be scoped to recompile.
local function mix(a: number, b: number): number
	return (a * 7 + b) % 1009
end

local function run(start: number)
	local trace = {}

	local function note(value)
		trace[#trace + 1] = tostring(value)
	end

	local seed = start

	do
		local first = mix(seed, 0)
		local second = mix(first, 0)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 1)
		local second = mix(first, 3)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 2)
		local second = mix(first, 6)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 3)
		local second = mix(first, 9)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 4)
		local second = mix(first, 12)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 5)
		local second = mix(first, 15)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 6)
		local second = mix(first, 18)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 7)
		local second = mix(first, 21)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 8)
		local second = mix(first, 24)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 9)
		local second = mix(first, 27)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 10)
		local second = mix(first, 30)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 11)
		local second = mix(first, 33)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 12)
		local second = mix(first, 36)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 13)
		local second = mix(first, 39)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 14)
		local second = mix(first, 42)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 15)
		local second = mix(first, 45)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 16)
		local second = mix(first, 48)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 17)
		local second = mix(first, 51)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 18)
		local second = mix(first, 54)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 19)
		local second = mix(first, 57)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 20)
		local second = mix(first, 60)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 21)
		local second = mix(first, 63)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 22)
		local second = mix(first, 66)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 23)
		local second = mix(first, 69)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 24)
		local second = mix(first, 72)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 25)
		local second = mix(first, 75)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 26)
		local second = mix(first, 78)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 27)
		local second = mix(first, 81)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 28)
		local second = mix(first, 84)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 29)
		local second = mix(first, 87)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 30)
		local second = mix(first, 90)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 31)
		local second = mix(first, 93)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 32)
		local second = mix(first, 96)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 33)
		local second = mix(first, 99)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 34)
		local second = mix(first, 102)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 35)
		local second = mix(first, 105)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 36)
		local second = mix(first, 108)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 37)
		local second = mix(first, 111)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 38)
		local second = mix(first, 114)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 39)
		local second = mix(first, 117)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 40)
		local second = mix(first, 120)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 41)
		local second = mix(first, 123)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 42)
		local second = mix(first, 126)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 43)
		local second = mix(first, 129)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 44)
		local second = mix(first, 132)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 45)
		local second = mix(first, 135)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 46)
		local second = mix(first, 138)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 47)
		local second = mix(first, 141)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 48)
		local second = mix(first, 144)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 49)
		local second = mix(first, 147)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 50)
		local second = mix(first, 150)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 51)
		local second = mix(first, 153)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 52)
		local second = mix(first, 156)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 53)
		local second = mix(first, 159)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 54)
		local second = mix(first, 162)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 55)
		local second = mix(first, 165)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 56)
		local second = mix(first, 168)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 57)
		local second = mix(first, 171)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 58)
		local second = mix(first, 174)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 59)
		local second = mix(first, 177)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 60)
		local second = mix(first, 180)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 61)
		local second = mix(first, 183)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 62)
		local second = mix(first, 186)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 63)
		local second = mix(first, 189)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 64)
		local second = mix(first, 192)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 65)
		local second = mix(first, 195)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 66)
		local second = mix(first, 198)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 67)
		local second = mix(first, 201)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 68)
		local second = mix(first, 204)
		note(first + second)
		seed = (seed + second) % 977
	end
	do
		local first = mix(seed, 69)
		local second = mix(first, 207)
		note(first + second)
		seed = (seed + second) % 977
	end

	-- Values that outlive any scope the decompiler may introduce.
	local carried = seed
	local captured = seed * 2
	local read_both = function()
		return captured + carried
	end

	do
		local only = mix(carried, 0)
		note(only)
	end
	do
		local only = mix(carried, 1)
		note(only)
	end
	do
		local only = mix(carried, 2)
		note(only)
	end
	do
		local only = mix(carried, 3)
		note(only)
	end
	do
		local only = mix(carried, 4)
		note(only)
	end
	do
		local only = mix(carried, 5)
		note(only)
	end
	do
		local only = mix(carried, 6)
		note(only)
	end
	do
		local only = mix(carried, 7)
		note(only)
	end
	do
		local only = mix(carried, 8)
		note(only)
	end
	do
		local only = mix(carried, 9)
		note(only)
	end
	do
		local only = mix(carried, 10)
		note(only)
	end
	do
		local only = mix(carried, 11)
		note(only)
	end
	do
		local only = mix(carried, 12)
		note(only)
	end
	do
		local only = mix(carried, 13)
		note(only)
	end
	do
		local only = mix(carried, 14)
		note(only)
	end
	do
		local only = mix(carried, 15)
		note(only)
	end
	do
		local only = mix(carried, 16)
		note(only)
	end
	do
		local only = mix(carried, 17)
		note(only)
	end
	do
		local only = mix(carried, 18)
		note(only)
	end
	do
		local only = mix(carried, 19)
		note(only)
	end
	do
		local only = mix(carried, 20)
		note(only)
	end
	do
		local only = mix(carried, 21)
		note(only)
	end
	do
		local only = mix(carried, 22)
		note(only)
	end
	do
		local only = mix(carried, 23)
		note(only)
	end
	do
		local only = mix(carried, 24)
		note(only)
	end
	do
		local only = mix(carried, 25)
		note(only)
	end
	do
		local only = mix(carried, 26)
		note(only)
	end
	do
		local only = mix(carried, 27)
		note(only)
	end
	do
		local only = mix(carried, 28)
		note(only)
	end
	do
		local only = mix(carried, 29)
		note(only)
	end
	do
		local only = mix(carried, 30)
		note(only)
	end
	do
		local only = mix(carried, 31)
		note(only)
	end
	do
		local only = mix(carried, 32)
		note(only)
	end
	do
		local only = mix(carried, 33)
		note(only)
	end
	do
		local only = mix(carried, 34)
		note(only)
	end
	do
		local only = mix(carried, 35)
		note(only)
	end
	do
		local only = mix(carried, 36)
		note(only)
	end
	do
		local only = mix(carried, 37)
		note(only)
	end
	do
		local only = mix(carried, 38)
		note(only)
	end
	do
		local only = mix(carried, 39)
		note(only)
	end
	do
		local only = mix(carried, 40)
		note(only)
	end
	do
		local only = mix(carried, 41)
		note(only)
	end
	do
		local only = mix(carried, 42)
		note(only)
	end
	do
		local only = mix(carried, 43)
		note(only)
	end
	do
		local only = mix(carried, 44)
		note(only)
	end
	do
		local only = mix(carried, 45)
		note(only)
	end
	do
		local only = mix(carried, 46)
		note(only)
	end
	do
		local only = mix(carried, 47)
		note(only)
	end
	do
		local only = mix(carried, 48)
		note(only)
	end
	do
		local only = mix(carried, 49)
		note(only)
	end
	do
		local only = mix(carried, 50)
		note(only)
	end
	do
		local only = mix(carried, 51)
		note(only)
	end
	do
		local only = mix(carried, 52)
		note(only)
	end
	do
		local only = mix(carried, 53)
		note(only)
	end
	do
		local only = mix(carried, 54)
		note(only)
	end
	do
		local only = mix(carried, 55)
		note(only)
	end
	do
		local only = mix(carried, 56)
		note(only)
	end
	do
		local only = mix(carried, 57)
		note(only)
	end
	do
		local only = mix(carried, 58)
		note(only)
	end
	do
		local only = mix(carried, 59)
		note(only)
	end
	do
		local only = mix(carried, 60)
		note(only)
	end
	do
		local only = mix(carried, 61)
		note(only)
	end
	do
		local only = mix(carried, 62)
		note(only)
	end
	do
		local only = mix(carried, 63)
		note(only)
	end
	do
		local only = mix(carried, 64)
		note(only)
	end
	do
		local only = mix(carried, 65)
		note(only)
	end
	do
		local only = mix(carried, 66)
		note(only)
	end
	do
		local only = mix(carried, 67)
		note(only)
	end
	do
		local only = mix(carried, 68)
		note(only)
	end
	do
		local only = mix(carried, 69)
		note(only)
	end

	note(carried)
	note(captured)
	note(read_both())

	-- A repeat condition reads a local declared in the body it closes over.
	local ticks = 0

	repeat
		local step = mix(ticks, 5)
		note(step)
		ticks += 1
		local finished = ticks >= 4
	until finished

	-- Closures escaping a loop body keep their captures alive.
	local escaped = {}

	for index = 1, 12 do
		local held = mix(index, seed)

		escaped[index] = function()
			return held
		end

		if held == 0 then
			break
		end
	end

	for index = 1, 12 do
		note(escaped[index]())
	end

	return table.concat(trace, ","), #trace, carried, captured, ticks
end

return run
