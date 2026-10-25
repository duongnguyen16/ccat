Provider icons for the Settings sidebar. The .svg files are the sources of truth.
png/ holds them rasterized once to 96x96 (transparent, centred, ~6% margin) because egui cannot render SVG.
ollama and openai-chatgpt PNGs are pure white on transparent so the app can tint them; the others keep brand colours.
