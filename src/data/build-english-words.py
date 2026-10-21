"""Builds src/data/english-words.fc from /usr/share/dict/web2.

Lower-case a-z words of 3 to 10 letters, sorted bytewise, unique, front-coded: each line is one
digit (letters shared with the previous word, 0-9) followed by the rest of the word. Every
RESTART-th line shares nothing, so a lookup can binary-search those lines.
"""
import re
import sys

RESTART = 64
src, out = sys.argv[1], sys.argv[2]
words = sorted({w for w in open(src).read().split() if re.fullmatch(r"[a-z]{3,10}", w)})
lines = []
prev = ""
for i, w in enumerate(words):
    shared = 0
    if i % RESTART:
        while shared < min(len(prev), len(w), 9) and prev[shared] == w[shared]:
            shared += 1
    lines.append(f"{shared}{w[shared:]}")
    prev = w
open(out, "w").write("\n".join(lines) + "\n")
print(len(words), sum(len(l) + 1 for l in lines))
