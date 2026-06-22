// Bundles the extension host code into a single CommonJS file for VS Code.
// The webview script (media/reader.js) is plain JS and is shipped as-is.
const esbuild = require('esbuild');

const production = process.argv.includes('--production');
const watch = process.argv.includes('--watch');

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: ['src/extension.ts'],
  bundle: true,
  outfile: 'dist/extension.js',
  platform: 'node',
  format: 'cjs',
  target: 'node18',
  external: ['vscode'],
  sourcemap: !production,
  minify: production,
  logLevel: 'info',
};

async function main() {
  if (watch) {
    const ctx = await esbuild.context(options);
    await ctx.watch();
    console.log('[tech-reader] watching…');
  } else {
    await esbuild.build(options);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
