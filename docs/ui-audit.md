# UI improvement: project-first navigation

Reference: Home's large project cards. Radar should feel like a place to visit
projects and work, not a collection of utility toolbars. Keep the user's theme
and keep developer tools explicit, rather than copying Basecamp's colours.

## Screen audit and changes

| Surface | Mismatch | Change |
| --- | --- | --- |
| Home | Small title and Agents hidden in a top-right count | Hero title with context below; a large Agents card with running count, explanation and drill-down arrow |
| Projects on Home | The strongest existing pattern | Preserve the card grid and inline actions; give cards more breathing room |
| Project board | Compact utility header and flat lane dividers | Shared horizontal hero/back header: title and context share a stack; project actions sit at the right; framed lane cards and individually surfaced to-dos |
| Card conversation | Project heading competes with a small task title; dense comments | Parent project becomes quiet context; the task owns the hero title; roomier conversation cards |
| Agents | Different compact header despite being a Home destination | Same hero/back header with the running count under the title and Auto arrange at the right; retain persistent live panes and their arrangement |
| Add project | Another custom toolbar header | Same hero/back header with Choose folder at the right; keep the existing search/add/create flow |
| Empty Home | Already project-first | Keep the single Add a project action |
| Developer workspace | Fixed kind-based splits and window-based nested-divider sizes waste available space | Shared responsive tiler with the global Agents workspace; keep compact pane headers and keyboard shortcuts |
| Preferences, edit dialogs and shortcuts | Focused secondary controls, not primary destinations | Keep them secondary; no new navigation or duplicated settings on Home |
| Remote web client | Separate, older sidebar-based surface | Aligned to this information architecture: Home's Basecamp lanes (Needs you, Agents, Projects), the project board, and the card conversation; see [`web-client.md`](web-client.md) |

## Navigation rules

- Home → project → to-do conversation; Home → Agents; Home → Add project.
- Visible Back text returns to the previous context, not necessarily Home.
- Home and board updates must not recreate interactive agent panes.
- A navigation card must be keyboard reachable. Inline to-do/session controls
  must retain their own actions rather than accidentally opening the parent.
- Hero titles wrap; board lanes may scroll horizontally rather than forcing
  the whole window wider.

## Review checklist

1. Home: Agents is discoverable without a toolbar; its empty state is reachable.
2. Open a project, then a to-do; Back returns through the same context.
3. Long project/task titles wrap; task title dominates the conversation.
4. Add project: search, Add/Create, Choose folder and Back still work.
5. Agents: typing and layout survive metadata updates and returning Home.
6. Narrow window: Home cards wrap and project columns remain scrollable.
7. Theme changes continue to recolour cards; terminals retain compact chrome.

## Workspace sizing follow-up

Both workspaces use one pure row planner, scored against terminal-shaped 8:5
panels using the actual content allocation. It distributes panels in stable
order, with equal row heights and equal panel widths within each row; the last
row stretches rather than leaving blank cells. Membership changes rebuild the
automatic arrangement; viewport changes only reparent panels if the row plan
changes. Existing terminal widgets/processes are retained. Nested dividers use
their own allocation, including the separator, instead of the window's size.

Manual header drags remain an override. Auto arrange clears that override and
stale divider positions (and exits a project-panel zoom), without reviving
dismissed global panels. Dividers retain their local proportions on resize.
With many panels, auto mode allows shrinking below comfortable minimum sizes
so it still fits; use project zoom for focused work.

Check with `bash scripts/layout-smoke.sh` and `bash scripts/home-smoke.sh`.
