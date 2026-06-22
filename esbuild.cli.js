// Bundles the terminal CLI (src/cli/index.ts) into a single executable Node file.
const esbuild = require('esbuild');
const fs = require('fs');

const production = process.argv.includes('--production');
const watch = process.argv.includes('--watch');

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: ['src/cli/index.ts'],
  bundle: true,
  outfile: 'dist/cli.js',
  platform: 'node',
  format: 'cjs',
  target: 'node18',
  banner: { js: '#!/usr/bin/env node' },
  sourcemap: !production,
  minify: production,
  logLevel: 'info',
};

async function main() {
  if (watch) {
    const ctx = await esbuild.context(options);
    await ctx.watch();
    console.log('[tech-reader cli] watching…');
  } else {
    await esbuild.build(options);
    try { fs.chmodSync('dist/cli.js', 0o755); } catch { /* ignore */ }
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
