# open-rust-chatbot mascot avatars v2: spec

Supersedes v1 icon set. Sections 4 (placement), 5 (accessibility), 6 (uploads) of v1 SPEC.md still apply unchanged, summarised in 4-6 below.

## 1. Character
One base character: flat 2D round-headed mascot. White head (r19 on 64 grid) on a navy disc (#0A2540), amber ring (#FFC24B, 2px). Large navy oval eyes with a white highlight, 3px rounded smile, two faint amber cheeks. No nose, no hair, no body, no gradients or shading. Reads as a character, not a person or logo.

Palette: navy #0A2540 (ground), white (head), amber #FFC24B (ring, accent), amber-dark #E8A317 (hat shade), mint #5FD3A5 / #3FB88A (secondary on roofer, locksmith, gardener, cleaner, directory). Contrast: amber ring on navy 8.9:1, so the disc and ring both separate from white pages (navy) and dark pages (amber ring, ~9:1 on #111827). Eyes/smile navy on white 15:1.

| file | name | props |
|---|---|---|
| default | neutral | amber chat-bubble antenna |
| electrician | "Sparky" | amber hard hat, navy bolt |
| plumber | | amber cap, spanner badge |
| heating | | amber beanie with pompom, flame badge |
| roofer | | mint hard hat, tile/roof badge |
| solar | | amber sun-visor band, sun badge |
| builder | | amber hard hat with ridge, trowel badge |
| locksmith | | mint cap, key badge |
| gardener | | mint seedling sprout (two leaves) |
| cleaner | | amber + mint sparkles, bucket badge |
| directory | "Ranky" | amber map-pin on head, #1 rosette badge with mint ribbons |

Badges (r6.5 amber circle, navy glyph) sit bottom-left so the wave hand has the right side clear.

## 2. Sizes and the -sm variant
- Full (`<name>.svg`): 40px (header), 56px (launcher), 64px. Minimum for props: 40px. Below that props turn to noise.
- `-sm` (`<name>-sm.svg`): face only, 24 to 32px (message-row avatars, typing indicator). Head r24.5 fills the disc, eyes rx4.6 / ry5.6, smile stroke 4.2 (about 1.6px at 24px), 3px ring. Props are intentionally dropped, so all 11 -sm files are the same face; ship one `default-sm.svg` and map every trade to it if you want fewer files (the per-name files exist so callers need no special-casing and so a future per-trade tint costs nothing).
- Do not use full art at 24px (preview column "24 full, avoid" shows why).
- Every file is under 1.7 KB.

## 3. Theming
Colours are fixed (no CSS variables) so `<img>` and inline behave identically. Brand-colour recolouring is dropped in v2: the navy and amber are the identity. If a site needs a different ground, change the first `fill` (disc) only and keep the amber ring.

## 4. Placement
| spot | variant | notes |
|---|---|---|
| Launcher | full, 56px in the existing 64px white button (4px padding) | decorative; button keeps its aria-label. Inline SVG if the wave is wanted, otherwise `<img>` |
| Header | full, 40px in `.orc-header-avatar` (white, 2px padding) | white ring on navy header |
| Message rows | `-sm`, 24px | bottom-aligned, 8px gap, first of a run, 32px spacer after; never next to user messages, notices, errors |
| Typing indicator | `-sm`, 24px | |
| Cards | none | |

## 5. Accessibility
- Header/message/launcher avatars are decorative: `alt=""` or `aria-hidden="true"`. Name is announced in the header text, not per avatar.
- Alt pattern where an alt is needed: "<bot name>, <business name>'s assistant" ("Sparky, Brightwire Electrical's assistant"); fallbacks "<business name>'s assistant", "Chat assistant".
- No `<title>` inside the SVG.
- Disclosure: the widget must keep the AI disclosure. The character is an obvious cartoon but never imply a human is typing.
- Launcher focus ring stays on the button (`:focus-visible`), avatar does not clip it.

## 6. Owner uploads (unchanged from v1)
Square crop, 128 to 512px, <=200KB, strip EXIF, re-encode raster server-side, sanitise or rasterise SVG, render only via `<img>`, `onerror` falls back to the bundled trade mascot. Photos of people: allowed as owner choice but disclosure copy stays, and dashboard should warn against implying a named human.

## 7. Disclosure greeting copy
Pattern (first assistant message, also fits the header subtitle):
`Hi, I'm {botName}, {Business}'s AI assistant. I can answer questions about {services} and pass your details to the team. How can I help?`
Examples:
- "Hi, I'm Sparky, Brightwire Electrical's AI assistant. I can answer questions about our electrical work and pass your details to the team. How can I help?"
- Directory: "Hi, I'm Ranky, Leeds Trades' AI assistant. I can help you find a local trade and send your enquiry on. What do you need?"
- No bot name: "Hi, I'm {Business}'s AI assistant. ..."
Rules: "AI assistant" must appear in the first message; do not use "my name is Dave" style human names; the existing `.orc-disclosure` strip stays. Header: title = Business, subtitle = "{botName}, AI assistant".

## 8. Default name mapping
electrician -> "Sparky"; directory -> "Ranky"; others default to the site's configured bot name, else "the assistant" (no invented human names).

## 9. Launcher animation
Files: `wave.css` (keyframes + rules). Needs inline SVG; with `<img>` the avatar is simply static (correct fallback).
- Wave: a mitten hand on an amber sleeve rises on the right, waves about 3 swings (-8 to +24 degrees around the wrist at 55,45), fades out. 1.8s, starts 2.5s after the class is added, `iteration-count: 1`, then static idle.
- Blink: eyes squash to scaleY .1 for ~0.25s; run twice only (first at ~16s, then 9s later), never continuous.
- Reduced motion: both rules sit inside `@media (prefers-reduced-motion: no-preference)`; the hand has `opacity="0"` as an attribute so it stays hidden without CSS.
- Once per session, in widget.js (trusted DOM, no innerHTML needed if the SVG is parsed with DOMParser from a bundled string):

```js
var root = document.querySelector('.orc-root');
try {
  if (!sessionStorage.getItem('orc-waved')) {
    sessionStorage.setItem('orc-waved', '1');
    root.classList.add('orc-wave-go');
  }
} catch (e) { /* storage blocked: skip animation */ }
```
Skip the wave when the panel is already open or the user has focused the launcher. Append the hand `<g>` (in the header comment of wave.css) to the full-size launcher SVG only.

## 10. QA status and known limits
- The author environment had no shell, so `qlmanage` PNG renders and visual checks could NOT be run: faces were verified by coordinate arithmetic only (eyes 2.5px wide and smile 1.6px thick at 24px sm; head, eyes and smile inside the disc and ring). Render and eyeball before shipping (`qlmanage -t -s 256 -o <dir> <file>.svg`, then open preview.html over `python3 -m http.server`).
- Likely weak points: badge glyphs (flame, trowel, key, #1) at 40px are 3 to 4px detail; the cleaner bucket and directory ribbons are the busiest; props are not legible below 40px by design.
- Amber on a white page has only ~1.6:1, but the navy disc carries the 3:1 non-text contrast on light pages, and the amber ring carries it on dark pages.
