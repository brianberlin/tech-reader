import * as vscode from 'vscode';

// A single shared Output channel ("Tech Reader") for host- and webview-side
// diagnostics. The webview can't write here directly, so it posts {type:'log'}
// messages that the player forwards to log(..., 'webview').

let channel: vscode.OutputChannel | undefined;

export function getChannel(): vscode.OutputChannel {
  if (!channel) channel = vscode.window.createOutputChannel('Tech Reader');
  return channel;
}

export function log(message: string, scope: 'host' | 'webview' | 'ollama' = 'host'): void {
  const ts = new Date().toISOString().slice(11, 23); // HH:MM:SS.mmm
  getChannel().appendLine(`${ts} [${scope}] ${message}`);
}

export function showLogs(): void {
  getChannel().show(true);
}

export function disposeChannel(): void {
  channel?.dispose();
  channel = undefined;
}
