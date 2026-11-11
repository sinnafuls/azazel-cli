-- word_count.luau: gives the agents a tool that counts the words of a file.
--
-- Install: copy this file to ~/.azazel/extensions/ (every project) or to
-- <project>/.azazel/extensions/ (one project, once you have trusted it), then run /reload.

-- azazel.tool{ ... } registers a tool. It can only be called here, while the script loads.
azazel.tool({
	-- The name the model calls the tool by: letters, digits, `_` and `-`.
	name = "word_count",
	-- The model decides from this text when to use the tool, so say what it does and returns.
	description = "Count the words in a text file of the project. Returns the number of words.",
	-- A JSON Schema for the arguments, written as a Luau table.
	parameters = {
		type = "object",
		properties = {
			path = { type = "string", description = "The file, relative to the project root" },
		},
		required = { "path" },
	},
	-- What the tool touches: "read", "write", "exec" (the default) or "net". The permission mode
	-- decides from this whether the user is asked first, and "read" calls may run at the same
	-- time as other read-only calls.
	access = "read",
	-- Called with the model's arguments and the call's context (ctx.cwd, ctx.session_id,
	-- ctx.agent_id, ctx.mode). Return a string, or { content = string, is_error = boolean }.
	execute = function(args, ctx)
		if type(args.path) ~= "string" or args.path == "" then
			return { content = "`path` must name a file", is_error = true }
		end
		-- azazel.fs reads and writes files in the project, the configured additional directories
		-- and this script's own data directory (azazel.fs.data_dir); any other path is an error.
		-- An error raised here, such as a missing file, reaches the model as an error result.
		local text = azazel.fs.read(args.path)
		local count = 0
		for _ in string.gmatch(text, "%S+") do
			count += 1
		end
		return tostring(count)
	end,
})
