--!strict
--[[
instructions — keep docs/TASKS.md as the standing record of work
instructions and their checklist state (the Luau port of pi's
`pi-instructions-ext`).

Stage 1 (this file): the file layer and the event hooks.

- `session_start`: inject a short pointer — when docs/TASKS.md or
  docs/RULES.md are missing, tell the model to call the `tasks_init` tool
  and then read docs/RULES.md; when both exist, tell it to read
  docs/RULES.md (the recording rules, generated once and user-editable).
  The pointer is a custom message, so a re-run (`/reload`) does not inject
  it twice.
- `before_agent_start`: append a short per-turn reminder (the identical
  string every turn so provider prompt caches stay valid); it switches to a
  staleness warning when docs/TASKS.md changed on disk since the agent last
  touched it (a parallel session or a manual edit), or to a rules-changed
  notice when docs/RULES.md changed.
- `turn_end`: refresh the staleness snapshot so the agent's own writes do
  not trigger the warning.
- `session_compact`: re-inject a short pointer — summaries may not preserve
  item-level checklist state.
- `tasks_init` tool: create docs/TASKS.md (a minimal skeleton) and
  docs/RULES.md (the recording rules) if missing; existing files are never
  overwritten.
- `tasks_tidy` tool: deterministic formatting — normalize checkbox syntax,
  indentation and "Confirm (user):" prefixes. Item order is preserved (no
  reordering); unrecognized lines pass through untouched; a file without
  checklist items is left as is.

Not ported yet (see docs/TASKS.md): the `/tasks-*` commands (the host
cannot invoke an extension command handler yet), `/tasks-blocked` /
`/tasks-completed` (needs `ctx.ui.custom` and `ctx.ui.get_editor_text`),
and `/tasks-archive` / `/tasks-clear` / `/tasks-verify` / `/tasks-info`.

divergences from pi's version:
- change detection uses `size:mtime_ms` from `pillar.fs.stat` (the Luau VM
  has no hashing library) instead of a SHA-1 of the contents.
- `TAG_DISPLAY = false` keeps the per-turn tag out of the transcript, like
  pi; it still rides in the session file.
]]

local pillar = require("@pillar")

-- --- Configuration -----------------------------------------------------------

local TASKS_PATH = "docs/TASKS.md"
local RULES_PATH = "docs/RULES.md"
local ARCHIVE_DIR = "docs/archives"

local TAG = "Auto-message: Please update docs/TASKS.md as needed."

local TAG_STALE = "Auto-message: "
	.. TASKS_PATH
	.. " has changed on disk since the agent last touched it — "
	.. "re-read it before updating (a parallel session or the user may have edited it). "
	.. "Merge the changes instead of overwriting them."

local RULES_STALE = "Auto-message: "
	.. RULES_PATH
	.. " has changed on disk since the agent last touched it — "
	.. "re-read it before continuing; the recording conventions may have changed."

-- Show the per-turn tag in the transcript. Hidden by default; still stored
-- in the session file.
local TAG_DISPLAY = false

local INIT_CUSTOM_TYPE = "instructions:init"
local TAG_CUSTOM_TYPE = "instructions:auto-tag"
local COMPACT_CUSTOM_TYPE = "instructions:post-compact"

local RULES_TEMPLATE = [[# TASKS recording rules

How this project records work instructions and their state (docs/TASKS.md).

## Recording

- Record each new instruction in docs/TASKS.md **before** starting on it,
  with a checklist of concrete steps as GFM checkboxes (`- [ ]` / `- [x]`).
- Keep the file up to date as work progresses. Skip pure acknowledgements
  ("yes", "continue") — judge what counts as an instruction.
- Respond and record in the user's language.

## User confirmation

- Items whose completion requires the user (behavior or visual checks,
  acceptance) get a sub-item prefixed "Confirm (user):" — never check it
  off yourself.
- When work finishes, ask the user about each pending confirmation item
  via the ask_user_question tool (one question per item, the concrete
  check described, options like OK / problem / later; plain text if the
  tool is unavailable). Never mark an item confirmed without an explicit
  user answer.
- Reflect answers as: "[x] … — user-confirmed (date)" · "— needs-fix:
  <summary>" · keep deferred items unchecked with "— pending
  confirmation".

## Parallel sessions

- Tag each top-level instruction you record with your session owner tag,
  e.g. "(s01a0)" (provided in the session-start message), and update only
  items carrying your own tag unless the user explicitly instructs
  otherwise.
- Re-read docs/TASKS.md before updating it — parallel sessions may have
  changed it. Merge, don't overwrite.

## Housekeeping

- If docs/TASKS.md accumulates many completed items, suggest that the user
  run /tasks-archive. The archive command is user-invoked — never move
  completed items yourself.
- When work finishes and the project has no git repository yet, suggest
  initializing one so the work and this record stay in history. At natural
  checkpoints, suggest committing uncommitted changes. The agent proposes —
  git is run only with the user's go-ahead.
- The user may feed items back into the conversation via /tasks-blocked
  (unfinished / pending-confirmation / needs-fix items) or /tasks-completed
  (checked items, for re-check requests). When the user quotes one of these
  items, find it in docs/TASKS.md and work on exactly that item.

<!-- Generated by instructions. Edit freely; the extension never
     overwrites this file once it exists. -->
]]

local TASKS_TEMPLATE = [[# TASKS

<!-- Work-instruction record. See docs/RULES.md for conventions. -->

## Active

## Completed
]]

-- --- Files -------------------------------------------------------------------

local function sessionPointer(ownerShort: string, filesReady: boolean): string
	if filesReady then
		return "Auto-message: Start by reading "
			.. RULES_PATH
			.. " — it defines how work instructions are recorded in "
			.. TASKS_PATH
			.. '. Your session owner tag is "(s'
			.. ownerShort
			.. ')".'
	end
	return "Auto-message: "
		.. TASKS_PATH
		.. " and "
		.. RULES_PATH
		.. " are not set up yet — call the tasks_init tool to generate them (it only creates what is missing), "
		.. "then read "
		.. RULES_PATH
		.. " and follow it when recording work instructions. "
		.. 'Your session owner tag is "(s'
		.. ownerShort
		.. ')".'
end

-- Create the template files if missing. Never overwrites existing files.
local function ensureTaskFiles(): (boolean, boolean)
	local createdTasks = not pillar.fs.exists(TASKS_PATH)
	local createdRules = not pillar.fs.exists(RULES_PATH)
	if createdTasks then
		pillar.fs.write(TASKS_PATH, TASKS_TEMPLATE)
	end
	if createdRules then
		pillar.fs.write(RULES_PATH, RULES_TEMPLATE)
	end
	return createdTasks, createdRules
end

-- --- Staleness snapshot (parallel-session change detection) -------------------

type Snapshot = { tasks: string?, rules: string? }

local lastKnown: Snapshot? = nil

-- A cheap content signature: the VM has no hashing library, so the size and
-- modification time stand in for the content hash.
local function signature(relPath: string): string?
	local info = pillar.fs.stat(relPath)
	if info == nil or info.type ~= "file" then
		return nil
	end
	return tostring(info.size) .. ":" .. tostring(info.modified_ms)
end

local function snapshot(): Snapshot
	return {
		tasks = signature(TASKS_PATH),
		rules = signature(RULES_PATH),
	}
end

-- --- Checklist parsing / formatting -------------------------------------------

type ItemNode = {
	level: number,
	checked: boolean,
	text: string,
	children: { ItemNode },
	trailing: { string },
}

type Section = {
	heading: string?,
	pre: { string },
	roots: { ItemNode },
}

local function indentWidth(raw: string): number
	local n = 0
	for i = 1, #raw do
		local ch = raw:sub(i, i)
		n += if ch == "\t" then 2 else 1
	end
	return n
end

-- Lua patterns cannot express the checkbox shape with `string.match` captures
-- as tersely as a regex, but the shape is fixed: optional indent, a bullet,
-- `[ ]` / `[x]`, then the text.
local function matchCheckbox(line: string): (string?, boolean?, string?)
	local indent, mark, text = line:match("^([ \t]*)[%-%*+][ \t]*%[([ xX])%][ \t]*(.*)$")
	if indent == nil or mark == nil or text == nil then
		return nil, nil, nil
	end
	return indent, mark:lower() == "x", text
end

-- Strip a "Confirm (user):" style prefix, answering the remaining text (nil
-- when the line has no such prefix). Handles the common variants
-- (`confirm(user)`, `Confirm (User) —`, full-width colon).
local function stripConfirmPrefix(text: string): string?
	if text:sub(1, 7):lower() ~= "confirm" then
		return nil
	end
	local rest = text:sub(8)
	rest = rest:gsub("^%s*", "")
	if rest:sub(1, 1) == "(" then
		rest = rest:sub(2)
		rest = rest:gsub("^%s*", "")
		if rest:sub(1, 4):lower() ~= "user" then
			return nil
		end
		rest = rest:sub(5)
		rest = rest:gsub("^%s*", "")
		if rest:sub(1, 1) == ")" then
			rest = rest:sub(2)
		end
	end
	rest = rest:gsub("^%s*", "")
	rest = rest:gsub("^[:：%-—]+", "")
	rest = rest:gsub("^%s*", "")
	return rest
end

local function parseDoc(text: string): { Section }
	local sections: { Section } = {}
	local current: Section = { heading = nil, pre = {}, roots = {} }
	table.insert(sections, current)
	local stack: { ItemNode } = {}

	for line in text:gmatch("([^\n]*)\n?") do
		if line == "" and text:sub(-1) ~= "\n" and #sections == 1 and #current.pre == 0 then
			-- A single empty document: nothing to attach.
			break
		end
		if line:match("^#+ ") then
			current = { heading = line, pre = {}, roots = {} }
			table.insert(sections, current)
			stack = {}
		else
			local indent, checked, itemText = matchCheckbox(line)
			if indent ~= nil and checked ~= nil and itemText ~= nil then
				local level = math.floor(indentWidth(indent) / 2)
				while #stack > 0 and stack[#stack].level >= level do
					table.remove(stack)
				end
				local node: ItemNode = {
					level = level,
					checked = checked,
					text = itemText:gsub("%s+$", ""),
					children = {},
					trailing = {},
				}
				if #stack > 0 then
					table.insert(stack[#stack].children, node)
				else
					table.insert(current.roots, node)
				end
				table.insert(stack, node)
			else
				-- Non-checkbox line: attach to the innermost open item so it
				-- travels with it; before any item in the section it is
				-- leading prose.
				if #stack > 0 then
					table.insert(stack[#stack].trailing, line)
				elseif #current.roots > 0 then
					table.insert(current.roots[#current.roots].trailing, line)
				else
					table.insert(current.pre, line)
				end
			end
		end
	end
	return sections
end

local function countItems(sections: { Section }): number
	local n = 0
	local function walk(node: ItemNode)
		n += 1
		for _, child in node.children do
			walk(child)
		end
	end
	for _, section in sections do
		for _, root in section.roots do
			walk(root)
		end
	end
	return n
end

local function renderNode(node: ItemNode, depth: number, out: { string }): ()
	local pad = string.rep("  ", depth)
	local text = node.text
	-- Normalize "Confirm (user):" prefixes on sub-items (common variants).
	if depth > 0 then
		local stripped = stripConfirmPrefix(text)
		if stripped ~= nil then
			text = "Confirm (user): " .. stripped
		end
	end
	local line = pad .. "- " .. (if node.checked then "[x]" else "[ ]") .. " " .. text
	line = line:gsub("%s+$", "")
	table.insert(out, line)
	for _, line in node.trailing do
		table.insert(out, line)
	end
	for _, child in node.children do
		renderNode(child, depth + 1, out)
	end
end

local function serializeDoc(sections: { Section }): string
	local out: { string } = {}
	for _, section in sections do
		if section.heading ~= nil then
			table.insert(out, section.heading)
		end
		for _, line in section.pre do
			table.insert(out, line)
		end
		for _, root in section.roots do
			renderNode(root, 0, out)
		end
	end
	return table.concat(out, "\n")
end

type TidyOutcome = {
	status: string, -- "missing" | "no-items" | "unchanged" | "changed"
	items: number,
	tidied: string?,
}

local function computeTidy(): TidyOutcome
	local original = pillar.fs.read(TASKS_PATH)
	if original == nil then
		return { status = "missing", items = 0 }
	end
	local sections = parseDoc(original)
	local items = countItems(sections)
	if items == 0 then
		return { status = "no-items", items = 0 }
	end
	local tidied = serializeDoc(sections)
	if tidied == original then
		return { status = "unchanged", items = items }
	end
	return { status = "changed", items = items, tidied = tidied }
end

local function writeTidied(tidied: string): ()
	pillar.fs.write(TASKS_PATH, tidied)
	lastKnown = snapshot()
end

-- --- Owner tag ----------------------------------------------------------------

-- Short session tag shown as "(s<tag>)" in the session-start pointer. Hashing
-- is unavailable in the VM, so the tag is derived from the last four
-- characters of the session id instead of a SHA-1 of the whole id (session
-- ids are UUIDv7, so the tail is the random part).
local function ownerShort(ctx: any): string
	local id = ctx.sessionManager.getSessionId()
	if type(id) ~= "string" or id == "" then
		return "sess"
	end
	local cleaned = id:gsub("%-", "")
	if #cleaned < 4 then
		return cleaned
	end
	return cleaned:sub(-4)
end

-- --- Tools --------------------------------------------------------------------

pillar.register_tool({
	name = "tasks_tidy",
	label = "Tidy tasks file",
	description = "Normalize "
		.. TASKS_PATH
		.. ': fix checkbox syntax, indentation and "Confirm (user):" prefixes. Order-preserving: no reordering. Non-destructive: unrecognized lines pass through untouched.',
	parameters = pillar.schema.object({}),
	execute = function(tool_call_id, params, signal, on_update, ctx)
		local outcome = computeTidy()
		if outcome.status == "missing" then
			return {
				content = { { type = "text", text = TASKS_PATH .. " does not exist." } },
				details = outcome,
			}
		end
		if outcome.status == "no-items" then
			return {
				content = { { type = "text", text = "No checklist items in " .. TASKS_PATH .. " — nothing to tidy." } },
				details = outcome,
			}
		end
		if outcome.status == "unchanged" then
			return {
				content = {
					{ type = "text", text = TASKS_PATH .. " is already tidy (" .. tostring(outcome.items) .. " items)." },
				},
				details = outcome,
			}
		end
		writeTidied(outcome.tidied :: string)
		return {
			content = {
				{
					type = "text",
					text = "Tidied "
						.. TASKS_PATH
						.. ": "
						.. tostring(outcome.items)
						.. ' item(s) reformatted (checkbox syntax, indentation, "Confirm (user):" prefixes). Item order preserved.',
				},
			},
			details = outcome,
		}
	end,
})

pillar.register_tool({
	name = "tasks_init",
	label = "Initialize tasks files",
	description = "Create "
		.. TASKS_PATH
		.. " (a minimal skeleton) and "
		.. RULES_PATH
		.. " (the recording rules) if they are missing. Never overwrites existing files. Read "
		.. RULES_PATH
		.. " afterwards and follow it.",
	parameters = pillar.schema.object({}),
	execute = function(tool_call_id, params, signal, on_update, ctx)
		local createdTasks, createdRules = ensureTaskFiles()
		local details: { createdTasks: boolean, createdRules: boolean } = {
			createdTasks = createdTasks,
			createdRules = createdRules,
		}
		if not createdTasks and not createdRules then
			return {
				content = {
					{
						type = "text",
						text = TASKS_PATH
							.. " and "
							.. RULES_PATH
							.. " already exist — read "
							.. RULES_PATH
							.. " and follow it.",
					},
				},
				details = details,
			}
		end
		local parts: { string } = {}
		if createdTasks then
			table.insert(parts, "created " .. TASKS_PATH .. " (skeleton)")
		end
		if createdRules then
			table.insert(parts, "created " .. RULES_PATH .. " (recording rules)")
		end
		return {
			content = {
				{
					type = "text",
					text = table.concat(parts, ", ")
						.. ". Read "
						.. RULES_PATH
						.. " and follow it when recording work instructions.",
				},
			},
			details = details,
		}
	end,
})

-- --- Event hooks --------------------------------------------------------------

-- One-time injection of the session pointer. `session_start` re-fires on
-- /reload, /resume and /fork — dedup by scanning for our custom message.
pillar.on("session_start", function(event, ctx)
	local entries = ctx.sessionManager.getEntries()
	local alreadyInjected = false
	for _, entry in entries do
		if entry.type == "custom_message" and entry.customType == INIT_CUSTOM_TYPE then
			alreadyInjected = true
			break
		end
	end
	if alreadyInjected then
		lastKnown = snapshot()
		return nil
	end

	local filesReady = pillar.fs.exists(TASKS_PATH) and pillar.fs.exists(RULES_PATH)
	pillar.send_message({
		customType = INIT_CUSTOM_TYPE,
		content = sessionPointer(ownerShort(ctx), filesReady),
		display = true,
	})

	-- Baseline for staleness detection — don't flag the state we just saw.
	lastKnown = snapshot()

	if ctx.hasUI then
		ctx.ui.notify("Tasks mode: " .. TASKS_PATH, "info")
	end
	return nil
end)

-- Per-turn reminder, riding next to the user's prompt. Identical text every
-- turn so provider prompt caches stay valid; it switches to a staleness
-- warning when a file changed on disk since the agent last touched it. Rules
-- changes take priority: the conventions govern how the record is kept.
pillar.on("before_agent_start", function(event, ctx)
	local current = snapshot()
	local tasksStale = false
	local rulesStale = false
	if lastKnown ~= nil then
		-- Only flag changes to files that currently exist: a file that never
		-- existed is not a change, and a deleted file is not flagged.
		tasksStale = current.tasks ~= nil and current.tasks ~= lastKnown.tasks
		rulesStale = current.rules ~= nil and current.rules ~= lastKnown.rules
		lastKnown = current
	else
		lastKnown = current
	end
	return {
		message = {
			customType = TAG_CUSTOM_TYPE,
			content = if rulesStale then RULES_STALE elseif tasksStale then TAG_STALE else TAG,
			display = TAG_DISPLAY,
		},
	}
end)

-- Refresh the snapshot at the end of every turn so the agent's own writes to
-- the file never trigger the staleness warning on the next turn.
pillar.on("turn_end", function(event, ctx)
	lastKnown = snapshot()
	return nil
end)

-- After context compaction the summary may not preserve item-level checklist
-- state — re-inject a short pointer so the model re-reads the file.
pillar.on("session_compact", function(event, ctx)
	if not pillar.fs.exists(TASKS_PATH) then
		return nil
	end
	pillar.send_message({
		customType = COMPACT_CUSTOM_TYPE,
		content = "Auto-message: The context was just compacted. The summary may not preserve item-level checklist state — "
			.. "re-read "
			.. TASKS_PATH
			.. " to restore the current state before updating the record, "
			.. "and keep following "
			.. RULES_PATH
			.. ".",
		display = false,
	})
	return nil
end)

-- The archive directory is only created by /tasks-archive (stage 2); keeping
-- the constant here documents the layout the port targets.
local _ARCHIVE_DIR = ARCHIVE_DIR

return nil
