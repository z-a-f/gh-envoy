use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use process_wrap::std::{ChildWrapper, CommandWrap};

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessSession;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioPolicy {
    Inherit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSpec {
    pub executable: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env_overrides: Vec<(OsString, Option<OsString>)>,
    pub stdio: StdioPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Exited(i32),
    Interrupted,
}

pub struct GroupedChild {
    child: Box<dyn ChildWrapper>,
}

impl ProcessSpec {
    pub fn spawn_grouped(&self) -> io::Result<GroupedChild> {
        let mut command = CommandWrap::with_new(&self.executable, |command| {
            command.args(&self.args).current_dir(&self.cwd);
            for (name, value) in &self.env_overrides {
                match value {
                    Some(value) => {
                        command.env(name, value);
                    }
                    None => {
                        command.env_remove(name);
                    }
                }
            }
            match self.stdio {
                StdioPolicy::Inherit => {
                    command
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit());
                }
            }
        });
        #[cfg(unix)]
        command.wrap(ProcessSession);
        #[cfg(windows)]
        command.wrap(JobObject);

        command.spawn().map(|child| GroupedChild { child })
    }
}

impl GroupedChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn wait_interruptibly(&mut self, interrupted: &AtomicBool) -> io::Result<WaitOutcome> {
        loop {
            if interrupted.load(Ordering::SeqCst) {
                self.child.start_kill()?;
                self.child.wait()?;
                return Ok(WaitOutcome::Interrupted);
            }
            if let Some(status) = self.child.try_wait()? {
                return Ok(WaitOutcome::Exited(exit_status_code(status)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn kill_and_wait(&mut self) -> io::Result<()> {
        self.child.start_kill()?;
        self.child.wait().map(|_| ())
    }
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}
