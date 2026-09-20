--!strict
--[[
instructions — keep docs/TASKS.md as the standing record of work
instructions and their checklist state (the Luau port of pi's
`pi-instructions-ext`).

Event hooks:

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

Tools:

- `tasks_init`: create docs/TASKS.md (a minimal skeleton) and
  docs/RULES.md (the recording rules) if missing; existing files are never
  overwritten.
- `tasks_tidy`: deterministic formatting — normalize checkbox syntax, flatten
  nested checklist items to the top level and normalize the
  "Confirm (user):" prefix. Item order is preserved (no reordering);
  unrecognized lines pass through untouched; a file without checklist items
  is left as is.

Commands:

- `/tasks-init` — generate the templates (never overwrites).
- `/tasks-tidy` — the same rewrite as the tool, behind a confirmation dialog.
- `/tasks-archive` — move every checked item (with its notes; output is
  flat) to a dated file under docs/archives/.
- `/tasks-clear` — clear the file after confirmation and regenerate the
  skeleton with a short tombstone line (date + cleared by the user); doubles
  as re-initialization.
- `/tasks-blocked` / `/tasks-completed` — two-column picker over the
  checklist (left: categories, ←/→; right: items, ↑/↓). Enter spawns the
  item's text into the editor; Esc closes.
- `/tasks-verify` — send the user message that starts the per-item
  confirmation walkthrough.
- `/tasks-info` — the `/tasks-*` cheat sheet.

divergences from pi's version:
- change detection uses `size:mtime_ms` from `pillar.fs.stat` (the Luau VM
  has no hashing library) instead of a SHA-1 of the contents.
- the session owner tag is the tail of the session id (UUIDv7) instead of a
  SHA-1 prefix, for the same reason.
- `TAG_DISPLAY = false` keeps the per-turn tag out of the transcript, like
  pi; it still rides in the session file.
- `visibleWidth` is a small East-Asian-width approximation (the picker only
  needs it for padding).
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

local VERIFY_MESSAGE = "Auto-message: Walk through every item in "
	.. TASKS_PATH
	.. ' that needs user confirmation (items marked "Confirm (user)", unfinished items whose completion condition is a user check such as behavior or visual verification, and any explicitly unfinished work). '
	.. "For each item, use the ask_user_question tool to ask the user in their language — one question per item, with the required check described concretely (e.g. what to run and what to look for) and options such as OK / problem / later. "
	.. "If the ask_user_question tool is unavailable, fall back to asking in plain text and waiting for the reply. "
	.. 'Reflect each answer in the file immediately: mark confirmed items "[x]" with a "— user-confirmed (date)" note, record problem reports as "— needs-fix: <summary>", and keep deferred items unchecked with "— pending confirmation". '
	.. "Do not mark anything confirmed without an explicit user answer. When the repository is git-tracked, suggest committing the file afterwards."

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

## Checklist shape

- Keep checklist items flat: one `- [ ]` / `- [x]` per line, never
  indented under another checklist item. /tasks-tidy flattens any nesting
  it finds.
- Write details (notes, rationale, sub-bullets) as plain non-checkbox lines
  under the item, indented as you like. They travel with the item when it is
  archived. A non-checkbox line that is not indented deeper than the item
  belongs to the section and stays in the file.
- "Confirm (user):" is its own flat item, not a sub-item.

## User confirmation

- Items whose completion requires the user (behavior or visual checks,
  acceptance) get their own item prefixed "Confirm (user):" — never check
  it off yourself.
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
  initializing one so the work stays in history. At natural checkpoints,
  suggest committing the work itself. The agent proposes — git is run only
  with the user's go-ahead.
- Never suggest a commit for docs/TASKS.md alone: it changes constantly as
  a record, and a dedicated commit is excessive. As long as the repository
  exists and the work is git-tracked, the record needs no extra attention.
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

-- --- Small text helpers -------------------------------------------------------

local function splitLines(text: string): { string }
	local lines: { string } = {}
	local start = 1
	while true do
		local nl = text:find("\n", start, true)
		if nl == nil then
			table.insert(lines, text:sub(start))
			break
		end
		table.insert(lines, text:sub(start, nl - 1))
		start = nl + 1
	end
	return lines
end

local function trimEnd(text: string): string
	return (text:gsub("%s+$", ""))
end

local function isBlank(text: string): boolean
	return text:match("^%s*$") ~= nil
end

-- --- Session pointer / files --------------------------------------------------

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

type ProseBlock = { kind: "prose", line: string }
type ItemNode = { kind: "item", level: number, checked: boolean, text: string, children: { any } }
type Block = ProseBlock | ItemNode
type Section = { heading: string?, blocks: { any } }

-- Leading indentation, tabs counting 2 (upstream `indentWidth`). Stops at the
-- first non-whitespace character.
local function indentWidth(raw: string): number
	local n = 0
	for i = 1, #raw do
		local ch = raw:sub(i, i)
		if ch == " " then
			n += 1
		elseif ch == "\t" then
			n += 2
		else
			break
		end
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

local function skipWhitespace(text: string, index: number): number
	local i = index
	while i <= #text do
		local ch = text:sub(i, i)
		if ch == " " or ch == "\t" then
			i += 1
		else
			break
		end
	end
	return i
end

-- The remainder after a "Confirm (user):" style prefix, or nil when the line
-- has no such prefix. Mirrors upstream's
-- `/^confirm\s*\(\s*user\s*\)?\s*[:：\-—]*\s*/i` (the closing paren and the
-- punctuation run are optional).
local function confirmPrefixRest(text: string): string?
	if text:sub(1, 7):lower() ~= "confirm" then
		return nil
	end
	local i = skipWhitespace(text, 8)
	if text:sub(i, i) ~= "(" then
		return nil
	end
	i = skipWhitespace(text, i + 1)
	if text:sub(i, i + 3):lower() ~= "user" then
		return nil
	end
	i = skipWhitespace(text, i + 4)
	if text:sub(i, i) == ")" then
		i += 1
	end
	i = skipWhitespace(text, i)
	while i <= #text do
		local ch = text:sub(i, i)
		if ch == ":" or ch == "-" then
			i += 1
		elseif text:sub(i, i + 2) == "：" or text:sub(i, i + 2) == "—" then
			i += 3
		else
			break
		end
	end
	return text:sub(skipWhitespace(text, i))
end

local function parseDoc(text: string): { Section }
	local sections: { Section } = {}
	local current: Section = { heading = nil, blocks = {} }
	table.insert(sections, current)
	local stack: { any } = {}

	local function push(block: any)
		if #stack > 0 then
			table.insert(stack[#stack].children, block)
		else
			table.insert(current.blocks, block)
		end
	end

	for _, rawLine in splitLines(text) do
		if rawLine:match("^#+ ") then
			current = { heading = rawLine, blocks = {} }
			table.insert(sections, current)
			stack = {}
		else
			local indent, checked, itemText = matchCheckbox(rawLine)
			if indent ~= nil and checked ~= nil and itemText ~= nil then
				local level = math.floor(indentWidth(indent) / 2)
				while #stack > 0 and stack[#stack].level >= level do
					table.remove(stack)
				end
				local node: any = {
					kind = "item",
					level = level,
					checked = checked,
					text = trimEnd(itemText),
					children = {},
				}
				push(node)
				table.insert(stack, node)
			else
				-- A non-checkbox line attaches to the innermost open item only
				-- when it is indented deeper than that item; shallower prose
				-- belongs to the section (so it never rides along into an
				-- archive). Blank lines never restructure the tree.
				if not isBlank(rawLine) then
					local noteLevel = math.floor(indentWidth(rawLine) / 2)
					while #stack > 0 and stack[#stack].level >= noteLevel do
						table.remove(stack)
					end
				end
				push({ kind = "prose", line = rawLine })
			end
		end
	end
	return sections
end

local function countItems(sections: { any }): number
	local n = 0
	local function walk(node: any)
		n += 1
		for _, child in node.children do
			if child.kind == "item" then
				walk(child)
			end
		end
	end
	for _, section in sections do
		for _, block in section.blocks do
			if block.kind == "item" then
				walk(block)
			end
		end
	end
	return n
end

local function countChecked(sections: { any }): number
	local n = 0
	local function walk(node: any)
		if node.checked then
			n += 1
		end
		for _, child in node.children do
			if child.kind == "item" then
				walk(child)
			end
		end
	end
	for _, section in sections do
		for _, block in section.blocks do
			if block.kind == "item" then
				walk(block)
			end
		end
	end
	return n
end

local function countCheckedInSubtree(node: any): number
	local n = if node.checked then 1 else 0
	for _, child in node.children do
		if child.kind == "item" then
			n += countCheckedInSubtree(child)
		end
	end
	return n
end

-- Render one item and its subtree. Checklist items are always emitted flat
-- (depth 0) — nesting is not a supported shape (see docs/RULES.md) and
-- /tasks-tidy flattens it. Prose lines are emitted verbatim, so their own
-- indentation is preserved.
local function renderNode(node: any, out: { string }): ()
	local text = node.text
	local rest = confirmPrefixRest(text)
	if rest ~= nil then
		text = "Confirm (user): " .. rest
	end
	local line = "- " .. (if node.checked then "[x]" else "[ ]") .. " " .. text
	table.insert(out, trimEnd(line))
	for _, child in node.children do
		if child.kind == "prose" then
			table.insert(out, child.line)
		else
			renderNode(child, out)
		end
	end
end

local function serializeDoc(sections: { any }): string
	local out: { string } = {}
	for _, section in sections do
		if section.heading ~= nil then
			table.insert(out, section.heading)
		end
		for _, block in section.blocks do
			if block.kind == "prose" then
				table.insert(out, block.line)
			else
				renderNode(block, out)
			end
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

-- --- Archiving -----------------------------------------------------------------

-- Whether a section still holds anything worth keeping.
local function blocksHaveContent(blocks: { any }): boolean
	for _, block in blocks do
		if block.kind == "item" or not isBlank(block.line) then
			return true
		end
	end
	return false
end

-- Move every checked item (at any nesting level) out of the sections. A moved
-- item keeps its whole subtree (including unchecked sub-items and notes). An
-- unchecked item stays put; checked descendants are extracted from it. Returns
-- the remaining sections and the rendered (flat) text of moved items. A
-- heading is dropped only when its section becomes completely empty (except a
-- level-1 document title).
type ArchiveOutcome = { remaining: { any }, movedText: string, movedCount: number }

local function archiveDoc(sections: { any }): ArchiveOutcome
	local moved: { any } = {}

	local function process(blocks: { any }): { any }
		local keep: { any } = {}
		for _, block in blocks do
			if block.kind == "prose" then
				table.insert(keep, block)
			elseif block.checked then
				table.insert(moved, block)
			else
				table.insert(keep, {
					kind = "item",
					level = block.level,
					checked = block.checked,
					text = block.text,
					children = process(block.children),
				})
			end
		end
		return keep
	end

	local remaining: { any } = {}
	for _, section in sections do
		local keep = process(section.blocks)
		if section.heading == nil then
			table.insert(remaining, { heading = nil, blocks = keep })
		elseif blocksHaveContent(keep) or section.heading:sub(1, 2) == "# " then
			table.insert(remaining, { heading = section.heading, blocks = keep })
		end
	end

	local out: { string } = {}
	local movedCount = 0
	for _, node in moved do
		renderNode(node, out)
		movedCount += countCheckedInSubtree(node)
	end
	return { remaining = remaining, movedText = table.concat(out, "\n"), movedCount = movedCount }
end

-- --- Time ---------------------------------------------------------------------

local dateFn: any = os.date

local function dateFormat(format: string): string
	local ok, value = pcall(dateFn, format)
	if ok and type(value) == "string" then
		return value
	end
	return ""
end

local function timestamp(): string
	return dateFormat("%Y-%m-%d %H:%M")
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

-- --- Checklist pickers (/tasks-blocked, /tasks-completed) ---------------------

type PickerItem = {
	label: string,
	checked: boolean,
	confirm: boolean,
	needsFix: boolean,
	depth: number,
	-- Lines spawned into the editor: ancestor chain + own text + direct children.
	spawn: { string },
}

-- Flatten every checklist item (any nesting level) into picker entries.
local function flattenSections(sections: { any }): { PickerItem }
	local out: { PickerItem } = {}

	local function walk(node: any, chain: { any }, depth: number)
		local lines: { any } = {}
		for _, ancestor in chain do
			table.insert(lines, ancestor)
		end
		table.insert(lines, { depth = depth, text = node.text })
		for _, child in node.children do
			if child.kind == "item" then
				table.insert(lines, { depth = depth + 1, text = child.text })
			end
		end
		local spawn: { string } = {}
		for _, line in lines do
			table.insert(spawn, string.rep("  ", line.depth) .. "- " .. line.text)
		end
		table.insert(out, {
			label = node.text,
			checked = node.checked,
			confirm = confirmPrefixRest(node.text) ~= nil,
			needsFix = node.text:lower():find("needs-fix", 1, true) ~= nil,
			depth = depth,
			spawn = spawn,
		})
		for _, child in node.children do
			if child.kind == "item" then
				local nextChain: { any } = {}
				for _, ancestor in chain do
					table.insert(nextChain, ancestor)
				end
				table.insert(nextChain, { depth = depth, text = node.text })
				walk(child, nextChain, depth + 1)
			end
		end
	end

	for _, section in sections do
		for _, block in section.blocks do
			if block.kind == "item" then
				walk(block, {}, 0)
			end
		end
	end
	return out
end

type PickerCategory = { name: string, items: { PickerItem } }

local PICKER_LEFT_WIDTH = 22
local PICKER_ROWS = 14

-- --- Display width (the picker pads columns) ----------------------------------

-- Display width of one codepoint; East Asian wide and emoji count as 2, the
-- combining/zero-width ranges as 0, everything else as 1. An approximation of
-- upstream's `visibleWidth` (no full Unicode tables in the VM).
local function codepointWidth(cp: number): number
	if cp >= 0x1100
		and (
			cp <= 0x115F
			or cp == 0x2329
			or cp == 0x232A
			or (cp >= 0x2E80 and cp <= 0xA4CF and cp ~= 0x303F)
			or (cp >= 0xAC00 and cp <= 0xD7A3)
			or (cp >= 0xF900 and cp <= 0xFAFF)
			or (cp >= 0xFE10 and cp <= 0xFE19)
			or (cp >= 0xFE30 and cp <= 0xFE6F)
			or (cp >= 0xFF00 and cp <= 0xFF60)
			or (cp >= 0xFFE0 and cp <= 0xFFE6)
			or (cp >= 0x1F300 and cp <= 0x1FAFF)
			or (cp >= 0x20000 and cp <= 0x3FFFD)
		)
	then
		return 2
	end
	if cp >= 0x0300 and cp <= 0x036F then
		return 0
	end
	if cp >= 0x200B and cp <= 0x200F then
		return 0
	end
	return 1
end

local function displayWidth(text: string): number
	local width = 0
	for _, cp in utf8.codes(text) do
		width += codepointWidth(cp)
	end
	return width
end

local function truncateToWidth(text: string, width: number): string
	if width <= 0 then
		return ""
	end
	if displayWidth(text) <= width then
		return text
	end
	local out: { string } = {}
	local used = 0
	for _, cp in utf8.codes(text) do
		local cw = codepointWidth(cp)
		if used + cw > width then
			break
		end
		used += cw
		table.insert(out, utf8.char(cp))
	end
	return table.concat(out)
end

local function padRight(text: string, width: number): string
	local pad = width - displayWidth(text)
	if pad <= 0 then
		return text
	end
	return text .. string.rep(" ", pad)
end

-- --- Picker component ---------------------------------------------------------

local TaskPicker = {}
TaskPicker.__index = TaskPicker

function TaskPicker.new(categories: any, title: string, theme: any, keybindings: any, done: any): any
	return setmetatable({
		categories = categories,
		title = title,
		theme = theme,
		keybindings = keybindings,
		done = done,
		cat_index = 1,
		item_index = 1,
		cat_scroll = 0,
		item_scroll = 0,
	}, TaskPicker)
end

local function adjustScroll(scroll: number, index: number, max: number): number
	if index < scroll then
		return index
	end
	if index >= scroll + max then
		return index - max + 1
	end
	return scroll
end

function TaskPicker:handle_input(data: string): ()
	local lastCat = #self.categories
	local cat = self.categories[self.cat_index]
	local items = (cat and cat.items) or {}
	local itemCount = #items
	if self.keybindings.matches(data, "escape") or self.keybindings.matches(data, "ctrl+c") then
		self.done(nil)
		return
	end

	local dirty = false
	if self.keybindings.matches(data, "left") then
		self.cat_index = if self.cat_index == 1 then lastCat else self.cat_index - 1
		self.item_index = 1
		dirty = true
	elseif self.keybindings.matches(data, "right") then
		self.cat_index = if self.cat_index == lastCat then 1 else self.cat_index + 1
		self.item_index = 1
		dirty = true
	elseif self.keybindings.matches(data, "up") then
		self.item_index = math.max(1, self.item_index - 1)
		dirty = true
	elseif self.keybindings.matches(data, "down") then
		if itemCount > 0 then
			self.item_index = math.min(itemCount, self.item_index + 1)
		end
		dirty = true
	elseif self.keybindings.matches(data, "return") then
		local item = items[self.item_index]
		if item ~= nil then
			-- Wrap in quotes so the agent sees an exact item reference; the
			-- user appends their own wording after the closing quote.
			self.done('"' .. table.concat(item.spawn, "\n") .. '"')
		else
			self.done(nil)
		end
		return
	end

	if not dirty then
		return
	end
	self.cat_scroll = adjustScroll(self.cat_scroll, self.cat_index - 1, PICKER_ROWS)
	self.item_scroll = adjustScroll(self.item_scroll, self.item_index - 1, PICKER_ROWS)
end

function TaskPicker:render(width: number): { string }
	local th = self.theme
	local leftWidth = math.min(PICKER_LEFT_WIDTH, math.max(10, width - 30))
	local rightWidth = math.max(20, width - leftWidth - 3)
	local cat = self.categories[self.cat_index]
	local items = (cat and cat.items) or {}
	local lines: { string } = {}

	table.insert(lines, "")
	table.insert(lines, " " .. th.fg("accent", th.bold(self.title)))
	local rule = th.fg("borderMuted", string.rep("─", math.max(0, width - 1)))
	table.insert(lines, rule)

	for row = 0, PICKER_ROWS - 1 do
		local catIndex = self.cat_scroll + row + 1
		local left = string.rep(" ", leftWidth)
		if catIndex <= #self.categories then
			local name = padRight(truncateToWidth(" " .. self.categories[catIndex].name, leftWidth), leftWidth)
			if catIndex == self.cat_index then
				left = th.bg("selectedBg", th.fg("text", name))
			else
				left = th.fg("muted", name)
			end
		end

		local itemIndex = self.item_scroll + row + 1
		local right = ""
		if #items == 0 then
			if row == 0 then
				right = th.fg("dim", "no items")
			end
		elseif itemIndex <= #items then
			local item = items[itemIndex]
			local mark = if item.checked then th.fg("success", "✓") else th.fg("dim", "○")
			local plainMark = if item.checked then "✓" else "○"
			local text = truncateToWidth(item.label, rightWidth - 6)
			local plain = " " .. plainMark .. " " .. text
			local padding = math.max(0, rightWidth - 1 - displayWidth(plain))
			local colored = " " .. mark .. " " .. text .. string.rep(" ", padding)
			if itemIndex == self.item_index then
				right = th.bg("selectedBg", colored)
			else
				right = colored
			end
		end

		table.insert(lines, left .. th.fg("borderMuted", " │ ") .. right)
	end

	table.insert(lines, rule)
	table.insert(lines, " " .. th.fg("dim", "←/→ category · ↑/↓ item · Enter: insert into editor · Esc: close"))
	return lines
end

function TaskPicker:invalidate(): ()
	-- Every paint repaints (the port's `tui.requestRender` is a no-op).
end

-- Open the picker; return the spawned text (and the factory's TUI handle).
local function openTaskPicker(ctx: any, categories: { PickerCategory }, title: string): (any, any)
	local capturedTui: any = nil
	local picked = ctx.ui.custom(function(tui: any, theme: any, keybindings: any, done: any)
		capturedTui = tui
		return TaskPicker.new(categories, title, theme, keybindings, done)
	end)
	return picked, capturedTui
end

local function spawnIntoEditor(ctx: any, text: string): ()
	local previous = ctx.ui.get_editor_text()
	if type(previous) ~= "string" then
		previous = ""
	end
	if isBlank(previous) then
		ctx.ui.set_editor_text(text)
	else
		local separator = if previous:sub(-1) == "\n" then "" else "\n"
		ctx.ui.set_editor_text(previous .. separator .. text)
	end
end

-- --- Tools --------------------------------------------------------------------

pillar.register_tool({
	name = "tasks_tidy",
	label = "Tidy tasks file",
	description = "Normalize "
		.. TASKS_PATH
		.. ': fix checkbox syntax, flatten nested checklist items to top level, and normalize "Confirm (user):" prefixes. Order-preserving: no reordering. Non-destructive: unrecognized lines pass through untouched.',
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
						.. ' item(s) reformatted (checkbox syntax, flat items, "Confirm (user):" prefixes). Item order preserved.',
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

-- --- Commands -----------------------------------------------------------------

-- /tasks-init — generate the templates if missing (never overwrites).
pillar.register_command("tasks-init", {
	description = "Generate " .. TASKS_PATH .. " and " .. RULES_PATH .. " if missing (never overwrites)",
	handler = function(args, ctx)
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end
		local createdTasks, createdRules = ensureTaskFiles()
		if not createdTasks and not createdRules then
			ctx.ui.notify(
				TASKS_PATH .. " and " .. RULES_PATH .. " already exist — nothing to generate.",
				"info"
			)
			return
		end
		if createdTasks then
			ctx.ui.notify("Created " .. TASKS_PATH .. " (skeleton).", "info")
		end
		if createdRules then
			ctx.ui.notify("Created " .. RULES_PATH .. " (recording rules).", "info")
		end
	end,
})

-- /tasks-tidy — same rewrite as the tool, behind a confirmation dialog.
pillar.register_command("tasks-tidy", {
	description = "Tidy " .. TASKS_PATH .. ": normalize format (order-preserving, no reordering)",
	handler = function(args, ctx)
		local outcome = computeTidy()
		if outcome.status == "missing" then
			ctx.ui.notify(TASKS_PATH .. " does not exist.", "warning")
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end
		if outcome.status == "no-items" then
			ctx.ui.notify("No checklist items found in " .. TASKS_PATH .. " — nothing to tidy.", "info")
			return
		end
		if outcome.status == "unchanged" then
			ctx.ui.notify(TASKS_PATH .. " is already tidy.", "info")
			return
		end

		local ok = ctx.ui.confirm(
			"Tidy tasks?",
			tostring(outcome.items)
				.. ' checklist item(s) will be reformatted (checkbox syntax, flat items, "Confirm (user):" prefixes). Item order is preserved — no reordering. Unrecognized lines pass through untouched. See git diff if tracked.'
		)
		if not ok then
			ctx.ui.notify("Cancelled.", "info")
			return
		end
		writeTidied(outcome.tidied :: string)
		ctx.ui.notify(TASKS_PATH .. " tidied.", "info")
	end,
})

-- /tasks-archive — move completed items to a dated file under docs/archives/.
pillar.register_command("tasks-archive", {
	description = "Move completed items from " .. TASKS_PATH .. " to " .. ARCHIVE_DIR .. "/",
	handler = function(args, ctx)
		local original = pillar.fs.read(TASKS_PATH)
		if original == nil then
			ctx.ui.notify(TASKS_PATH .. " does not exist.", "warning")
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end

		local sections = parseDoc(original)
		local planned = countChecked(sections)
		if planned == 0 then
			ctx.ui.notify("No completed items in " .. TASKS_PATH .. " — nothing to archive.", "info")
			return
		end

		local ok = ctx.ui.confirm(
			"Archive completed items?",
			tostring(planned)
				.. " completed item(s) (with their notes) will be moved to "
				.. ARCHIVE_DIR
				.. "/ under a dated heading. See git diff if tracked."
		)
		if not ok then
			ctx.ui.notify("Cancelled.", "info")
			return
		end

		local outcome = archiveDoc(sections)

		local day = dateFormat("%Y-%m-%d")
		local time = dateFormat("%H:%M")
		local archiveFile = ARCHIVE_DIR .. "/TASKS-" .. day .. ".md"
		local block = "## Archived "
			.. day
			.. " "
			.. time
			.. "\n\n"
			.. trimEnd(outcome.movedText)
			.. "\n"
		local previous = pillar.fs.read(archiveFile)
		if previous ~= nil then
			if not previous:match("\n$") then
				previous = previous .. "\n"
			end
			pillar.fs.write(archiveFile, previous .. "\n" .. block)
		else
			pillar.fs.write(archiveFile, "# TASKS archive\n\n" .. block)
		end

		pillar.fs.write(TASKS_PATH, serializeDoc(outcome.remaining))
		lastKnown = snapshot()
		ctx.ui.notify(
			tostring(outcome.movedCount)
				.. " item(s) archived to "
				.. ARCHIVE_DIR
				.. "/TASKS-"
				.. day
				.. ".md. When git-tracked, suggest committing both files.",
			"info"
		)
	end,
})

-- /tasks-verify — inject a user message that starts the verification
-- walkthrough. The model asks the user per-item questions and reflects the
-- answers in the checklist file.
pillar.register_command("tasks-verify", {
	description = "Verify pending items in "
		.. TASKS_PATH
		.. " with the user (asks per-item questions, then updates the file)",
	handler = function(args, ctx)
		if not pillar.fs.exists(TASKS_PATH) then
			ctx.ui.notify(
				TASKS_PATH
					.. " does not exist — nothing to verify. It is created when the first instruction is recorded.",
				"warning"
			)
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end
		pillar.send_user_message(VERIFY_MESSAGE)
		ctx.ui.notify(
			"Verification request sent — the agent will ask about each item needing user confirmation.",
			"info"
		)
	end,
})

-- /tasks-blocked — pick an unfinished item and spawn its text into the
-- editor, so the user can pin-point it back to the agent ("this one — get on
-- with it") without retyping it.
pillar.register_command("tasks-blocked", {
	description = "Pick an unfinished item from "
		.. TASKS_PATH
		.. " into the editor (categories: unfinished / pending confirmation / needs-fix)",
	handler = function(args, ctx)
		if not pillar.fs.exists(TASKS_PATH) then
			ctx.ui.notify(
				TASKS_PATH .. " does not exist — it is created when the first instruction is recorded.",
				"warning"
			)
			return
		end
		if not ctx.hasUI then
			ctx.ui.notify("tasks-blocked needs the interactive UI.", "warning")
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end

		local original = pillar.fs.read(TASKS_PATH)
		if original == nil then
			return
		end
		local all = flattenSections(parseDoc(original))
		local unfinished: { PickerItem } = {}
		local pending: { PickerItem } = {}
		local needsFix: { PickerItem } = {}
		for _, item in all do
			if item.depth == 0 and not item.checked and not item.confirm and not item.needsFix then
				table.insert(unfinished, item)
			end
			if not item.checked and item.confirm then
				table.insert(pending, item)
			end
			if item.needsFix then
				table.insert(needsFix, item)
			end
		end
		local categories: { PickerCategory } = {}
		if #unfinished > 0 then
			table.insert(categories, { name = "Unfinished", items = unfinished })
		end
		if #pending > 0 then
			table.insert(categories, { name = "Pending confirm", items = pending })
		end
		if #needsFix > 0 then
			table.insert(categories, { name = "needs-fix", items = needsFix })
		end
		if #categories == 0 then
			ctx.ui.notify("No unfinished items in " .. TASKS_PATH .. ".", "info")
			return
		end

		local picked, tui = openTaskPicker(ctx, categories, TASKS_PATH)
		if picked ~= nil then
			spawnIntoEditor(ctx, picked)
			if tui ~= nil and tui.requestRender ~= nil then
				tui.requestRender()
			end
		end
	end,
})

-- /tasks-completed — same picker over checked items, for re-check requests
-- ("this one — double-check it again").
pillar.register_command("tasks-completed", {
	description = "Pick a completed item from " .. TASKS_PATH .. " into the editor (for re-check requests)",
	handler = function(args, ctx)
		if not pillar.fs.exists(TASKS_PATH) then
			ctx.ui.notify(
				TASKS_PATH .. " does not exist — it is created when the first instruction is recorded.",
				"warning"
			)
			return
		end
		if not ctx.hasUI then
			ctx.ui.notify("tasks-completed needs the interactive UI.", "warning")
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end

		local original = pillar.fs.read(TASKS_PATH)
		if original == nil then
			return
		end
		local all = flattenSections(parseDoc(original))
		local completed: { PickerItem } = {}
		for _, item in all do
			if item.checked then
				table.insert(completed, item)
			end
		end
		if #completed == 0 then
			ctx.ui.notify("No completed items in " .. TASKS_PATH .. ".", "info")
			return
		end

		local categories: { PickerCategory } = { { name = "Completed", items = completed } }
		local picked, tui = openTaskPicker(ctx, categories, TASKS_PATH)
		if picked ~= nil then
			spawnIntoEditor(ctx, picked)
			if tui ~= nil and tui.requestRender ~= nil then
				tui.requestRender()
			end
		end
	end,
})

-- /tasks-info — the command cheat sheet.
pillar.register_command("tasks-info", {
	description = "Show /tasks-* commands for " .. TASKS_PATH,
	handler = function(args, ctx)
		ctx.ui.notify(
			"Usage: /tasks-init — generate "
				.. TASKS_PATH
				.. "/"
				.. RULES_PATH
				.. " if missing · /tasks-blocked — pick an unfinished item into the editor · /tasks-completed — pick a completed item into the editor · /tasks-tidy — normalize format (order-preserving) · /tasks-archive — move completed items to "
				.. ARCHIVE_DIR
				.. "/ · /tasks-verify — walk pending confirmations with the user · /tasks-clear — clear "
				.. TASKS_PATH
				.. " and regenerate the skeleton with a tombstone note.",
			"info"
		)
	end,
})

-- /tasks-clear — clear the file and regenerate the skeleton with a short
-- tombstone line. Doubles as re-initialization: what remains is the template
-- plus one line recording when the user cleared it.
pillar.register_command("tasks-clear", {
	description = "Clear " .. TASKS_PATH .. " and regenerate the skeleton (tombstone line kept)",
	handler = function(args, ctx)
		local current = pillar.fs.read(TASKS_PATH)
		if current == nil then
			ctx.ui.notify(TASKS_PATH .. " does not exist.", "warning")
			return
		end
		if isBlank(current) then
			ctx.ui.notify(TASKS_PATH .. " is already empty.", "info")
			return
		end
		if not ctx.isIdle() then
			ctx.ui.notify("Agent is running. Try again after it settles.", "warning")
			return
		end

		local ok = ctx.ui.confirm(
			"Clear tasks?",
			TASKS_PATH
				.. " will be cleared and regenerated as the empty skeleton, keeping one short tombstone line (date + cleared by the user). This cannot be undone (see git history if tracked)."
		)
		if not ok then
			ctx.ui.notify("Cancelled.", "info")
			return
		end

		local tombstone = "<!-- Cleared by the user via /tasks-clear on "
			.. timestamp()
			.. (if pillar.fs.exists(".git") then " See git history if tracked." else "")
			.. " -->\n"
		pillar.fs.write(TASKS_PATH, TASKS_TEMPLATE .. "\n" .. tombstone)
		lastKnown = snapshot()
		ctx.ui.notify(TASKS_PATH .. " cleared and re-initialized.", "info")
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

return nil
