//! cargo_check provides the functionality needed to run `cargo check` or
//! another compatible command (f.x. clippy) in a background thread and provide
//! LSP diagnostics based on the output of the command.

use std::{
    fmt,
    io::{self, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use cargo_metadata::Message;
use crossbeam_channel::{never, select, unbounded, Receiver, RecvError, Sender};

pub use cargo_metadata::diagnostic::{
    Applicability, Diagnostic, DiagnosticLevel, DiagnosticSpan, DiagnosticSpanMacroExpansion,
};

type Progress = ra_progress::Progress<(), String>;
type ProgressSource = ra_progress::ProgressSource<(), String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlycheckConfig {
    CargoCommand {
        command: String,
        all_targets: bool,
        all_features: bool,
        features: Vec<String>,
        extra_args: Vec<String>,
    },
    CustomCommand {
        command: String,
        args: Vec<String>,
    },
}

impl fmt::Display for FlycheckConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlycheckConfig::CargoCommand { command, .. } => write!(f, "cargo {}", command),
            FlycheckConfig::CustomCommand { command, args } => {
                write!(f, "{} {}", command, args.join(" "))
            }
        }
    }
}

/// Flycheck wraps the shared state and communication machinery used for
/// running `cargo check` (or other compatible command) and providing
/// diagnostics based on the output.
/// The spawned thread is shut down when this struct is dropped.
#[derive(Debug)]
pub struct Flycheck {
    // XXX: drop order is significant
    cmd_send: Sender<CheckCommand>,
    handle: jod_thread::JoinHandle<()>,
    pub task_recv: Receiver<CheckTask>,
}

impl Flycheck {
    pub fn new(
        config: FlycheckConfig,
        workspace_root: PathBuf,
        progress_src: ProgressSource,
    ) -> Flycheck {
        let (task_send, task_recv) = unbounded::<CheckTask>();
        let (cmd_send, cmd_recv) = unbounded::<CheckCommand>();
        let handle = jod_thread::spawn(move || {
            FlycheckThread::new(config, workspace_root, progress_src).run(&task_send, &cmd_recv);
        });
        Flycheck { task_recv, cmd_send, handle }
    }

    /// Schedule a re-start of the cargo check worker.
    pub fn update(&self) {
        self.cmd_send.send(CheckCommand::Update).unwrap();
    }
}

#[derive(Debug)]
pub enum CheckTask {
    /// Request a clearing of all cached diagnostics from the check watcher
    ClearDiagnostics,

    /// Request adding a diagnostic with fixes included to a file
    AddDiagnostic { workspace_root: PathBuf, diagnostic: Diagnostic },
}

pub enum CheckCommand {
    /// Request re-start of check thread
    Update,
}

struct FlycheckThread {
    config: FlycheckConfig,
    workspace_root: PathBuf,
    last_update_req: Option<Instant>,
    progress_src: ProgressSource,
    progress: Option<Progress>,
    // XXX: drop order is significant
    message_recv: Receiver<CheckEvent>,
    /// WatchThread exists to wrap around the communication needed to be able to
    /// run `cargo check` without blocking. Currently the Rust standard library
    /// doesn't provide a way to read sub-process output without blocking, so we
    /// have to wrap sub-processes output handling in a thread and pass messages
    /// back over a channel.
    check_process: Option<jod_thread::JoinHandle<()>>,
}

impl FlycheckThread {
    fn new(
        config: FlycheckConfig,
        workspace_root: PathBuf,
        progress_src: ProgressSource,
    ) -> FlycheckThread {
        FlycheckThread {
            config,
            workspace_root,
            progress_src,
            last_update_req: None,
            progress: None,
            message_recv: never(),
            check_process: None,
        }
    }

    fn run(&mut self, task_send: &Sender<CheckTask>, cmd_recv: &Receiver<CheckCommand>) {
        // If we rerun the thread, we need to discard the previous check results first
        self.clean_previous_results(task_send);

        loop {
            select! {
                recv(&cmd_recv) -> cmd => match cmd {
                    Ok(cmd) => self.handle_command(cmd),
                    Err(RecvError) => {
                        // Command channel has closed, so shut down
                        break;
                    },
                },
                recv(self.message_recv) -> msg => match msg {
                    Ok(msg) => self.handle_message(msg, task_send),
                    Err(RecvError) => {
                        // Watcher finished, replace it with a never channel to
                        // avoid busy-waiting.
                        self.message_recv = never();
                        self.check_process = None;
                    },
                }
            };

            if self.should_recheck() {
                self.last_update_req = None;
                task_send.send(CheckTask::ClearDiagnostics).unwrap();
                self.restart_check_process();
            }
        }
    }

    fn clean_previous_results(&mut self, task_send: &Sender<CheckTask>) {
        task_send.send(CheckTask::ClearDiagnostics).unwrap();
        self.progress = None;
    }

    fn should_recheck(&mut self) -> bool {
        if let Some(_last_update_req) = &self.last_update_req {
            // We currently only request an update on save, as we need up to
            // date source on disk for cargo check to do it's magic, so we
            // don't really need to debounce the requests at this point.
            return true;
        }
        false
    }

    fn handle_command(&mut self, cmd: CheckCommand) {
        match cmd {
            CheckCommand::Update => self.last_update_req = Some(Instant::now()),
        }
    }

    fn handle_message(&mut self, msg: CheckEvent, task_send: &Sender<CheckTask>) {
        match msg {
            CheckEvent::Begin => {
                self.progress = Some(self.progress_src.begin(()));
            }
            CheckEvent::End => self.progress = None,
            CheckEvent::Msg(Message::CompilerArtifact(msg)) => {
                self.progress
                    .as_mut()
                    .expect("check process reported progress without the 'Begin' notification")
                    .report(msg.target.name);
            }

            CheckEvent::Msg(Message::CompilerMessage(msg)) => {
                task_send
                    .send(CheckTask::AddDiagnostic {
                        workspace_root: self.workspace_root.clone(),
                        diagnostic: msg.message,
                    })
                    .unwrap();
            }

            CheckEvent::Msg(Message::BuildScriptExecuted(_msg)) => {}
            CheckEvent::Msg(Message::BuildFinished(_)) => {}
            CheckEvent::Msg(Message::TextLine(_)) => {}
            CheckEvent::Msg(Message::Unknown) => {}
        }
    }

    fn restart_check_process(&mut self) {
        // First, clear and cancel the old thread
        self.message_recv = never();
        self.check_process = None;

        let mut cmd = match &self.config {
            FlycheckConfig::CargoCommand {
                command,
                all_targets,
                all_features,
                extra_args,
                features,
            } => {
                let mut cmd = Command::new(ra_toolchain::cargo());
                cmd.arg(command);
                cmd.args(&["--workspace", "--message-format=json", "--manifest-path"])
                    .arg(self.workspace_root.join("Cargo.toml"));
                if *all_targets {
                    cmd.arg("--all-targets");
                }
                if *all_features {
                    cmd.arg("--all-features");
                } else if !features.is_empty() {
                    cmd.arg("--features");
                    cmd.arg(features.join(" "));
                }
                cmd.args(extra_args);
                cmd
            }
            FlycheckConfig::CustomCommand { command, args } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                cmd
            }
        };
        cmd.current_dir(&self.workspace_root);

        let (message_send, message_recv) = unbounded();
        self.message_recv = message_recv;
        self.check_process = Some(jod_thread::spawn(move || {
            // If we trigger an error here, we will do so in the loop instead,
            // which will break out of the loop, and continue the shutdown
            let _ = message_send.send(CheckEvent::Begin);

            let res = run_cargo(cmd, &mut |message| {
                // Skip certain kinds of messages to only spend time on what's useful
                match &message {
                    Message::CompilerArtifact(artifact) if artifact.fresh => return true,
                    Message::BuildScriptExecuted(_) => return true,
                    Message::Unknown => return true,
                    _ => {}
                }

                // if the send channel was closed, we want to shutdown
                message_send.send(CheckEvent::Msg(message)).is_ok()
            });

            if let Err(err) = res {
                // FIXME: make the `message_send` to be `Sender<Result<CheckEvent, CargoError>>`
                // to display user-caused misconfiguration errors instead of just logging them here
                log::error!("Cargo watcher failed {:?}", err);
            }

            // We can ignore any error here, as we are already in the progress
            // of shutting down.
            let _ = message_send.send(CheckEvent::End);
        }))
    }
}

enum CheckEvent {
    Begin,
    Msg(cargo_metadata::Message),
    End,
}

fn run_cargo(
    mut command: Command,
    on_message: &mut dyn FnMut(cargo_metadata::Message) -> bool,
) -> io::Result<()> {
    let mut child =
        command.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn()?;

    // We manually read a line at a time, instead of using serde's
    // stream deserializers, because the deserializer cannot recover
    // from an error, resulting in it getting stuck, because we try to
    // be resillient against failures.
    //
    // Because cargo only outputs one JSON object per line, we can
    // simply skip a line if it doesn't parse, which just ignores any
    // erroneus output.
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut read_at_least_one_message = false;
    for message in cargo_metadata::Message::parse_stream(stdout) {
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                log::error!("Invalid json from cargo check, ignoring ({})", err);
                continue;
            }
        };

        read_at_least_one_message = true;

        if !on_message(message) {
            break;
        }
    }

    // It is okay to ignore the result, as it only errors if the process is already dead
    let _ = child.kill();

    let exit_status = child.wait()?;
    if !exit_status.success() && !read_at_least_one_message {
        // FIXME: Read the stderr to display the reason, see `read2()` reference in PR comment:
        // https://github.com/rust-analyzer/rust-analyzer/pull/3632#discussion_r395605298
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "the command produced no valid metadata (exit code: {:?}): {:?}",
                exit_status, command
            ),
        ));
    }

    Ok(())
}
