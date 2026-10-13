// Make the light-theme copy of the logo: only the silver lettering, its edge and the
// subtitle are darkened so they read on a light background; the icon and the blue "M"
// stay as designed. usage: node tools/logo_variants.js <logo.svg> <out-dark.svg> <out-light.svg>
const fs = require("fs");
const [src, outDark, outLight] = process.argv.slice(2);
const svg = fs.readFileSync(src, "utf8");
const recolor = {
  "silver-lettering": { "#F4F7FF": "#1A2643", "#E5EEFC": "#22335A", "#A6C3F0": "#2C4886" },
  "letter-edge": { "#FFFFFF": "#1A2643", "#7CB3FF": "#2C5BBE" },
  "subtitle-fill": { "#E1E9F9": "#3B4866", "#98B3E4": "#56698F" },
  "subtitle-local": { "#E1E9F9": "#3B4866", "#98B3E4": "#56698F" },
};
let light = svg;
for (const [id, map] of Object.entries(recolor)) {
  const start = light.indexOf(`id="${id}"`);
  if (start < 0) throw new Error(`gradient ${id} not found`);
  const open = light.lastIndexOf("<", start);
  const end = light.indexOf("Gradient>", start) + "Gradient>".length;
  let block = light.slice(open, end);
  for (const [from, to] of Object.entries(map)) block = block.split(`stop-color="${from}"`).join(`stop-color="${to}"`);
  light = light.slice(0, open) + block + light.slice(end);
}
fs.writeFileSync(outDark, svg);
fs.writeFileSync(outLight, light);
console.log("wrote", outDark, outLight);
