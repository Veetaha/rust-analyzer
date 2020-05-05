import * as vscode from 'vscode';
import * as lc from 'vscode-languageclient';
import * as ra from '../rust-analyzer-api';
import * as os from "os";

import { Ctx, Cmd } from '../ctx';
import { Cargo } from '../cargo';

export function run(ctx: Ctx): Cmd {
    let prevRunnable: RunnableQuickPick | undefined;

    return async () => {
        const editor = ctx.activeRustEditor;
        const client = ctx.client;
        if (!editor || !client) return;

        const textDocument: lc.TextDocumentIdentifier = {
            uri: editor.document.uri.toString(),
        };

        const runnables = await client.sendRequest(ra.runnables, {
            textDocument,
            position: client.code2ProtocolConverter.asPosition(
                editor.selection.active,
            ),
        });
        const items: RunnableQuickPick[] = [];
        if (prevRunnable) {
            items.push(prevRunnable);
        }
        for (const r of runnables) {
            if (
                prevRunnable &&
                JSON.stringify(prevRunnable.runnable) === JSON.stringify(r)
            ) {
                continue;
            }
            items.push(new RunnableQuickPick(r));
        }
        const item = await vscode.window.showQuickPick(items);
        if (!item) return;

        item.detail = 'rerun';
        prevRunnable = item;
        const task = createTask(item.runnable);
        return await vscode.tasks.executeTask(task);
    };
}

export function runSingle(ctx: Ctx): Cmd {
    return async (runnable: ra.Runnable) => {
        const editor = ctx.activeRustEditor;
        if (!editor) return;

        const task = createTask(runnable);
        task.group = vscode.TaskGroup.Build;
        task.presentationOptions = {
            reveal: vscode.TaskRevealKind.Always,
            panel: vscode.TaskPanelKind.Dedicated,
            clear: true,
        };

        return vscode.tasks.executeTask(task);
    };
}

function getLldbDebugConfig(config: ra.Runnable, sourceFileMap: Record<string, string>): vscode.DebugConfiguration {
    return {
        type: "lldb",
        request: "launch",
        name: config.label,
        cargo: {
            args: config.args,
        },
        args: config.extraArgs,
        cwd: config.cwd,
        sourceMap: sourceFileMap
    };
}

const debugOutput = vscode.window.createOutputChannel("Debug");

async function getCppvsDebugConfig(config: ra.Runnable, sourceFileMap: Record<string, string>): Promise<vscode.DebugConfiguration> {
    debugOutput.clear();

    const cargo = new Cargo(config.cwd || '.', debugOutput);
    const executable = await cargo.executableFromArgs(config.args);

    // if we are here, there were no compilation errors.
    return {
        type: (os.platform() === "win32") ? "cppvsdbg" : 'cppdbg',
        request: "launch",
        name: config.label,
        program: executable,
        args: config.extraArgs,
        cwd: config.cwd,
        sourceFileMap: sourceFileMap,
    };
}

export function debugSingle(ctx: Ctx): Cmd {
    return async (config: ra.Runnable) => {
        const editor = ctx.activeRustEditor;
        if (!editor) return;

        const lldbId = "vadimcn.vscode-lldb";
        const cpptoolsId = "ms-vscode.cpptools";

        const debugEngineId = ctx.config.debug.engine;
        let debugEngine = null;
        if (debugEngineId === "auto") {
            debugEngine = vscode.extensions.getExtension(lldbId);
            if (!debugEngine) {
                debugEngine = vscode.extensions.getExtension(cpptoolsId);
            }
        }
        else {
            debugEngine = vscode.extensions.getExtension(debugEngineId);
        }

        if (!debugEngine) {
            vscode.window.showErrorMessage(
                `Install [CodeLLDB](https://marketplace.visualstudio.com/items?itemName=${lldbId}) ` +
                `or [MS C++ tools](https://marketplace.visualstudio.com/items?itemName=${cpptoolsId}) ` +
                `extension for debugging.`
            );
            return;
        }

        const debugConfig = lldbId === debugEngine.id
            ? getLldbDebugConfig(config, ctx.config.debug.sourceFileMap)
            : await getCppvsDebugConfig(config, ctx.config.debug.sourceFileMap);

        return vscode.debug.startDebugging(undefined, debugConfig);
    };
}

class RunnableQuickPick implements vscode.QuickPickItem {
    public label: string;
    public description?: string | undefined;
    public detail?: string | undefined;
    public picked?: boolean | undefined;

    constructor(public runnable: ra.Runnable) {
        this.label = runnable.label;
    }
}

interface CargoTaskDefinition extends vscode.TaskDefinition {
    type: 'cargo';
    label: string;
    command: string;
    args: string[];
    env?: { [key: string]: string };
}

function createTask(spec: ra.Runnable): vscode.Task {
    const TASK_SOURCE = 'Rust';
    const definition: CargoTaskDefinition = {
        type: 'cargo',
        label: spec.label,
        command: spec.bin,
        args: spec.extraArgs ? [...spec.args, '--', ...spec.extraArgs] : spec.args,
        env: spec.env,
    };

    const execOption: vscode.ShellExecutionOptions = {
        cwd: spec.cwd || '.',
        env: definition.env,
    };
    const exec = new vscode.ShellExecution(
        definition.command,
        definition.args,
        execOption,
    );

    const f = vscode.workspace.workspaceFolders![0];
    const t = new vscode.Task(
        definition,
        f,
        definition.label,
        TASK_SOURCE,
        exec,
        ['$rustc'],
    );
    t.presentationOptions.clear = true;
    return t;
}
