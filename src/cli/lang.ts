import * as path from 'path';

// Map a file extension to the languageId that blocks.ts/narrator expect.
const MAP: Record<string, string> = {
  '.md': 'markdown', '.markdown': 'markdown', '.mdx': 'mdx',
  '.txt': 'plaintext', '.text': 'plaintext',
  '.rst': 'restructuredtext', '.adoc': 'asciidoc', '.asciidoc': 'asciidoc',
  '.ts': 'typescript', '.tsx': 'typescriptreact', '.mts': 'typescript', '.cts': 'typescript',
  '.js': 'javascript', '.jsx': 'javascriptreact', '.mjs': 'javascript', '.cjs': 'javascript',
  '.py': 'python', '.rb': 'ruby', '.go': 'go', '.rs': 'rust',
  '.java': 'java', '.kt': 'kotlin', '.kts': 'kotlin', '.scala': 'scala',
  '.c': 'c', '.h': 'c', '.cpp': 'cpp', '.cc': 'cpp', '.cxx': 'cpp', '.hpp': 'cpp', '.hh': 'cpp',
  '.cs': 'csharp', '.php': 'php', '.swift': 'swift', '.dart': 'dart',
  '.sh': 'shellscript', '.bash': 'shellscript', '.zsh': 'shellscript',
  '.sql': 'sql', '.lua': 'lua', '.hs': 'haskell', '.pl': 'perl', '.r': 'r',
  '.yaml': 'yaml', '.yml': 'yaml', '.toml': 'toml', '.json': 'json',
  '.html': 'html', '.htm': 'html', '.css': 'css', '.scss': 'scss',
  '.vue': 'vue', '.svelte': 'svelte',
};

export function languageIdForPath(filePath: string): string {
  const base = path.basename(filePath).toLowerCase();
  if (base === 'dockerfile') return 'dockerfile';
  if (base === 'makefile') return 'makefile';
  return MAP[path.extname(filePath).toLowerCase()] || 'plaintext';
}
