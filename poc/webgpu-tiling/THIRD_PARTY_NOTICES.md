# Omarchy theme palettes

The 22 Omarchy palettes in `themes.js` are derived from
[omacom/omarchy](https://github.com/omacom/omarchy/tree/9f066b649e0b5ac70e4ca1ba4dfbd523260a550c/themes),
revision `9f066b649e0b5ac70e4ca1ba4dfbd523260a550c`, using each theme’s `colors.toml`.
UI surfaces use those named colors; terminal palettes follow Omarchy’s Ghostty mapping.
The picker implementation and previews are original Boomux code.
No website implementation, wallpapers, fonts, or logos are included from Omarchy.

## MIT license

Copyright (c) David Heinemeier Hansson

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

## Bundled terminal font

`fonts/jetbrains-mono-nerd.woff2` is JetBrainsMono Nerd Font Regular from
`ttf-jetbrains-mono-nerd-basic` 3.5.1-1, converted losslessly from the installed
TTF to WOFF2 with `woff2_compress`. Copyright 2020 The JetBrains Mono Project
Authors. Distributed under SIL Open Font License 1.1; see `fonts/OFL.txt`.
The font is loaded as “Boomux Terminal” in CSS to avoid local-font ambiguity.
