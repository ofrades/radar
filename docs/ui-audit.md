# UI improvement: project-first navigation

Reference: Home's large project cards. Radar should feel like a place to visit
projects and work, not a collection of utility toolbars. Keep the user's theme
and keep developer tools explicit, rather than copying Basecamp's colours.

## Screen audit and changes

| Surface | Mismatch | Change |
| --- | --- | --- |
| Home | Small title and Agents hidden in a top-right count | Hero title with context below; a large Agents card with running count, explanation and drill-down arrow |
| Projects on Home | The strongest existing pattern | Preserve the card grid and inline actions; give cards more breathing room |
| Project board | Compact utility header and flat lane dividers | Shared hero/back header, framed lane cards and individually surfaced to-dos |
| Card conversation | Project heading competes with a small task title; dense comments | Parent project becomes quiet context; the task owns the hero title; roomier conversation cards |
| Agents | Different compact header despite being a Home destination | Same hero/back header; retain persistent live panes and their arrangement |
| Add project | Another custom toolbar header | Same hero/back header with an explicit Choose folder action; keep the existing search/add/create flow |
| Empty Home | Already project-first | Keep the single Add a project action |
| Developer workspace | Dense controls are useful inside a tool | Keep compact pane headers and keyboard shortcuts; entered explicitly from the project |
| Preferences, edit dialogs and shortcuts | Focused secondary controls, not primary destinations | Keep them secondary; no new navigation or duplicated settings on Home |
| Remote web client | Separate, older sidebar-based surface | Not changed in this native pass; aligning its information architecture is a follow-up |

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
