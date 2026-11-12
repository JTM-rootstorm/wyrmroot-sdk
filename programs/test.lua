-- SPDX-License-Identifier: GPL-3.0-or-later
-- The Lua interpreter's test on Wyrmroot: the language, its libraries, and
-- the C library under them (stdio, files on /, time, setjmp through pcall,
-- float formatting, and memory through the collector). Any failed check is
-- an error, so the interpreter exits with status 1.

local checks = 0
local function check(condition, what)
  checks = checks + 1
  if not condition then error("check failed: " .. what, 2) end
end

-- Arithmetic and number formatting.
check(7 // 2 == 3 and 7 % 3 == 1 and 7 / 2 == 3.5, "integer and float division")
check(math.type(1) == "integer" and math.type(1.0) == "float", "number subtypes")
check(string.format("%.5f", math.pi) == "3.14159", "float formatting")
check(string.format("%g", 1e308 * 10) == "inf" and 1e308 * 10 == math.huge, "infinity")
check(tonumber("0x1p4") == 16.0 and tonumber("  42  ") == 42, "number parsing")
check(math.maxinteger + 1 == math.mininteger, "integer wraparound")
check(math.abs(math.sin(math.pi / 2) - 1) < 1e-12 and math.sqrt(2) ^ 2 - 2 < 1e-12, "libm")

-- Strings, patterns and packing.
check(("hello world"):upper() == "HELLO WORLD", "upper")
check(("a,b,,c"):gsub(",", ";") == "a;b;;c", "gsub")
check(select(2, ("key = value"):match("^(%w+)%s*=%s*(%w+)$")) == "value", "match")
local words = {}
for word in ("the quick brown fox"):gmatch("%a+") do words[#words + 1] = word end
check(#words == 4 and words[3] == "brown", "gmatch")
check(string.unpack("<i4", string.pack("<i4", -2)) == -2, "pack")
check(utf8.len("wyrm\u{1F409}") == 5 and utf8.char(0x1F409) == "\u{1F409}", "utf8")

-- Tables, sorting and closures.
local list = {}
for i = 1, 1000 do list[i] = (i * 7919) % 1000 end
table.sort(list)
local sorted = true
for i = 2, #list do sorted = sorted and list[i - 1] <= list[i] end
check(sorted and #list == 1000, "sort")
check(table.concat({1, 2, 3}, "-") == "1-2-3", "concat")
local function counter()
  local count = 0
  return function() count = count + 1; return count end
end
local next_count = counter()
next_count(); next_count()
check(next_count() == 3, "closures")

-- Metatables.
local vector = {}
vector.__index = vector
vector.__add = function(a, b) return setmetatable({x = a.x + b.x}, vector) end
vector.__tostring = function(v) return "v(" .. v.x .. ")" end
check(tostring(setmetatable({x = 1}, vector) + setmetatable({x = 2}, vector)) == "v(3)",
      "metamethods")

-- Errors unwind through setjmp/longjmp.
local ok, message = pcall(function() error({code = 42}) end)
check(not ok and message.code == 42, "pcall with a table error")
ok, message = pcall(function() return nil + 1 end)
check(not ok and message:find("arithmetic") ~= nil, "runtime error")
check(select(2, xpcall(function() error("deep") end, function(m) return "handled: " .. m end))
      :find("handled") ~= nil, "xpcall")

-- Coroutines.
local generator = coroutine.wrap(function()
  for i = 1, 3 do coroutine.yield(i * i) end
end)
check(generator() == 1 and generator() == 4 and generator() == 9, "coroutines")

-- Memory: build and drop about a megabyte, then collect. (A job's heap is
-- about 4 MiB until Deepwyrm maps larger arenas.)
local before = collectgarbage("count")
do
  local junk = {}
  for i = 1, 8000 do junk[i] = string.rep("x", 100) .. i end
  check(#junk[8000] == 104, "allocation")
end
collectgarbage("collect")
check(collectgarbage("count") < before + 1024, "collection")

-- Files on the root filesystem.
local path = "/lua-test.txt"
local file = assert(io.open(path, "w"))
for i = 1, 100 do file:write("line ", i, "\n") end
file:close()
local lines = 0
for line in io.lines(path) do
  lines = lines + 1
  if lines == 100 then check(line == "line 100", "last line") end
end
check(lines == 100, "read back 100 lines")
file = assert(io.open(path, "r"))
check(file:seek("end") == 9 * 7 + 90 * 8 + 9, "seek to end")
file:close()
check(os.remove(path) and io.open(path, "r") == nil, "remove")
check(select(3, io.open("/no/such/file")) == 2, "ENOENT")

-- Time. Until request W6 delivers the boot epoch, the wall clock counts
-- from 1970 at boot; CPU-time clocks (os.clock) are not in the slice.
local start = os.time()
check(start >= 0 and os.difftime(os.time(), start) >= 0, "wall clock")
check(os.date("!%Y-%m-%d", 0) == "1970-01-01", "date formatting")
check(os.getenv("NO_SUCH_VARIABLE") == nil, "environment")

print(string.format("lua tests passed: %d checks, %s", checks, _VERSION))
