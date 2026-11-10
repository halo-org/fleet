use core::{
    pin::pin,
    task::{Context, Poll, ready},
};
use rustix::process::{self, Pid, Signal, WaitId, WaitIdOptions};
use std::{io, process::ExitStatus};
use tokio::{
    process::{Child, Command},
    signal::unix::{self, SignalKind},
};

pub struct ProcessGroup {
    child: Child,
    group: Option<Pid>,
    signal: unix::Signal,
}

impl ProcessGroup {
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        let signal = unix::signal(SignalKind::child())?;
        let child = command.process_group(0).spawn()?;
        let pid = child.id().expect("new child has a process ID");
        let group = Pid::from_raw(i32::try_from(pid).expect("process ID fits in i32"))
            .expect("new child has a positive process ID");
        Ok(Self {
            child,
            group: Some(group),
            signal,
        })
    }

    pub const fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn kill(&mut self) -> io::Result<()> {
        if let Some(group) = self.group {
            match process::kill_process_group(group, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => self.group = None,
                Err(error) => return Err(error.into()),
            }
        }
        self.child.start_kill()
    }

    pub fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        drop(self.child.stdin.take());
        while let Some(group) = self.group {
            match process::waitid(
                WaitId::Pid(group),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => self.kill()?,
                Ok(None) => {
                    if ready!(self.signal.poll_recv(cx)).is_none() {
                        return Poll::Ready(Err(io::Error::other("process signal stream closed")));
                    }
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Poll::Ready(Err(error.into())),
            }
        }
        pin!(self.child.wait()).poll(cx)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        drop(self.kill());
    }
}
