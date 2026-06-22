import * as vscode from 'vscode';
import * as path from 'path';
import { PlayerPanel, type OpenRequest } from './player/playerPanel';
import { log, showLogs, disposeChannel } from './log';

export function activate(context: vscode.ExtensionContext) {
  log(`Tech Reader activated (vscode ${vscode.version}).`);
  const reg = (id: string, fn: (...a: any[]) => any) =>
    context.subscriptions.push(vscode.commands.registerCommand(id, fn));

  reg('techReader.readDocument', () => { log('command: readDocument'); readDocument(context); });
  reg('techReader.readSelection', () => { log('command: readSelection'); readSelection(context); });
  reg('techReader.readFromCursor', () => { log('command: readFromCursor'); readFromCursor(context); });
  reg('techReader.stop', () => PlayerPanel.current?.control('stop'));
  reg('techReader.togglePlayPause', () => {
    if (PlayerPanel.current) PlayerPanel.current.control('playpause');
    else vscode.window.showInformationMessage('Tech Reader: nothing is playing. Run "Tech Reader: Read File" to start.');
  });
  reg('techReader.toggleMode', () => PlayerPanel.current?.toggleMode());
  reg('techReader.openPlayer', () => readDocument(context));
  reg('techReader.showLogs', () => showLogs());
}

function titleFor(uri: vscode.Uri): string {
  return path.basename(uri.fsPath) || 'Document';
}

function requireEditor(): vscode.TextEditor | undefined {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showErrorMessage('Tech Reader: open a file first.');
    return undefined;
  }
  return editor;
}

function baseRequest(doc: vscode.TextDocument, source: string, baseLine: number): OpenRequest {
  return {
    source,
    baseLine,
    lang: doc.languageId,
    docUri: doc.uri,
    docKey: doc.uri.toString(),
    title: titleFor(doc.uri),
  };
}

function readDocument(context: vscode.ExtensionContext) {
  const editor = requireEditor();
  if (!editor) return;
  const doc = editor.document;
  const source = doc.getText();
  if (!source.trim()) {
    vscode.window.showInformationMessage('Tech Reader: this file is empty.');
    return;
  }
  PlayerPanel.show(context).open(baseRequest(doc, source, 1));
}

function readSelection(context: vscode.ExtensionContext) {
  const editor = requireEditor();
  if (!editor) return;
  if (editor.selection.isEmpty) return readFromCursor(context);
  const doc = editor.document;
  const source = doc.getText(editor.selection);
  if (!source.trim()) {
    vscode.window.showInformationMessage('Tech Reader: nothing readable in the selection.');
    return;
  }
  const req = baseRequest(doc, source, editor.selection.start.line + 1);
  req.title = `${titleFor(doc.uri)} (selection)`;
  req.docKey = ''; // selections don't persist a resume position
  PlayerPanel.show(context).open(req);
}

function readFromCursor(context: vscode.ExtensionContext) {
  const editor = requireEditor();
  if (!editor) return;
  const doc = editor.document;
  const startLine = editor.selection.active.line;
  const range = new vscode.Range(startLine, 0, doc.lineCount, 0);
  const source = doc.getText(range);
  if (!source.trim()) {
    vscode.window.showInformationMessage('Tech Reader: nothing readable below the cursor.');
    return;
  }
  PlayerPanel.show(context).open(baseRequest(doc, source, startLine + 1));
}

export function deactivate() {
  PlayerPanel.current?.dispose();
  disposeChannel();
}
