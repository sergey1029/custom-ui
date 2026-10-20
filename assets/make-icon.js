// Draws assets/icon.ico (the app icon): a rounded tile with a blue-violet
// gradient and a white crescent. Run with `node assets/make-icon.js`.
const fs = require("fs");
const path = require("path");

function render(size) {
  const px = new Float32Array(size * size * 4); // premultiplied RGBA, 0..1
  const ss = 4; // supersampling per axis
  const r = size * 0.225; // corner radius
  const inset = size * 0.04;
  const moon = { x: size * 0.5, y: size * 0.5, r: size * 0.26 };
  const bite = { x: size * 0.62, y: size * 0.4, r: size * 0.22 };
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let tile = 0, white = 0;
      for (let sy = 0; sy < ss; sy++) {
        for (let sx = 0; sx < ss; sx++) {
          const fx = x + (sx + 0.5) / ss, fy = y + (sy + 0.5) / ss;
          const lo = inset, hi = size - inset;
          const cx = Math.min(Math.max(fx, lo + r), hi - r), cy = Math.min(Math.max(fy, lo + r), hi - r);
          const inTile = fx >= lo && fx <= hi && fy >= lo && fy <= hi && Math.hypot(fx - cx, fy - cy) <= r;
          if (!inTile) continue;
          tile++;
          const inMoon = Math.hypot(fx - moon.x, fy - moon.y) <= moon.r && Math.hypot(fx - bite.x, fy - bite.y) > bite.r;
          if (inMoon) white++;
        }
      }
      const n = ss * ss, a = tile / n, w = white / n;
      const t = (x + y) / (2 * size);
      // #1E6BFF → #8A3FFC diagonally
      const g = [0x1e + (0x8a - 0x1e) * t, 0x6b + (0x3f - 0x6b) * t, 0xff + (0xfc - 0xff) * t].map(v => v / 255);
      const i = (y * size + x) * 4;
      for (let c = 0; c < 3; c++) px[i + c] = g[c] * (a - w) + w; // white over gradient
      px[i + 3] = a;
    }
  }
  return px;
}

// One BMP-in-ICO image: BITMAPINFOHEADER, bottom-up BGRA, empty AND mask.
function bmp(size) {
  const px = render(size);
  const maskRow = Math.ceil(size / 32) * 4;
  const buf = Buffer.alloc(40 + size * size * 4 + maskRow * size);
  buf.writeUInt32LE(40, 0);
  buf.writeInt32LE(size, 4);
  buf.writeInt32LE(size * 2, 8);
  buf.writeUInt16LE(1, 12);
  buf.writeUInt16LE(32, 14);
  let o = 40;
  for (let y = size - 1; y >= 0; y--) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4, a = px[i + 3];
      const un = c => (a > 0 ? Math.round((px[i + c] / a) * 255) : 0);
      buf[o++] = un(2); buf[o++] = un(1); buf[o++] = un(0); buf[o++] = Math.round(a * 255);
    }
  }
  return buf;
}

const sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const images = sizes.map(bmp);
const header = Buffer.alloc(6 + 16 * sizes.length);
header.writeUInt16LE(1, 2);
header.writeUInt16LE(sizes.length, 4);
let offset = header.length;
sizes.forEach((s, k) => {
  const e = 6 + 16 * k;
  header[e] = s === 256 ? 0 : s;
  header[e + 1] = s === 256 ? 0 : s;
  header.writeUInt16LE(1, e + 4);
  header.writeUInt16LE(32, e + 6);
  header.writeUInt32LE(images[k].length, e + 8);
  header.writeUInt32LE(offset, e + 12);
  offset += images[k].length;
});
fs.writeFileSync(path.join(__dirname, "icon.ico"), Buffer.concat([header, ...images]));
