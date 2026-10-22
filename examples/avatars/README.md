# Example mascot avatars

A set of friendly mascot avatars for the chat widget: one character (round face, big eyes,
moustache, smile) with a hat or prop for different trades. They are plain, hand-authored SVGs
(paths only, no scripts, fonts or external references, about 1 KB each) and are released under
this repository's MIT licence, so you can use or adapt them for your own bots.

| File | Character |
| --- | --- |
| `default.svg` | Chat-bubble antenna |
| `electrician.svg` | Hard hat with a lightning bolt |
| `plumber.svg` | Cap with a spanner badge |
| `heating.svg` | Beanie with a flame badge |
| `roofer.svg` | Hard hat with a roof-tile badge |
| `solar.svg` | Visor with a sun badge |
| `builder.svg` | Hard hat with a trowel badge |
| `locksmith.svg` | Cap with a key badge |
| `gardener.svg` | Leaves on the head |
| `cleaner.svg` | Sparkles and a bucket badge |
| `directory.svg` | Map pin and a #1 rosette, for directory sites |

Each has a `-sm.svg` face-only version for small sizes (24–32px, e.g. beside each bot message),
where hats and badges would blur. Use the full versions at 40px and above (chat header, launcher).

## Why a mascot

Research on chatbot avatars points the same way: a friendly cartoon character with a face does
better on trust and willingness to chat than an abstract icon, while realistic human faces can
feel eerie and over-promise. Round faces and large eyes read as approachable, and cuteness makes
people more forgiving of small mistakes. Keep it competent rather than childish, and always say
it's an AI in the greeting, for example: "Hi, I'm Sparky, Acme Electrical's AI assistant. I can
answer questions about rewires and EV chargers, or pass your details to the team."

## Files

- `preview.html` shows every avatar at 24–64px on light and dark backgrounds and in a mock widget.
  Serve the folder (`python3 -m http.server`) to see the launcher wave; `file://` shows static images.
- `wave.css` is an optional one-time launcher wave and occasional blink. Both only run under
  `prefers-reduced-motion: no-preference`, never loop, and need the SVG inlined (an `<img>` stays static).
- `SPEC.md` covers placement and sizes, alt text, contrast, and rules for owner-uploaded avatars.

A built-in avatar option for the widget (pick one of these per bot, or upload your own) is planned.
Until then you can use them in your own launcher or page design.
