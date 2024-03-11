use std::{
	io::{Error, Result},
	ops::ControlFlow,
	os::unix::process::{CommandExt, ExitStatusExt},
	process::{Child, Command, ExitStatus},
};

use nix::{
	errno::Errno,
	libc,
	sys::{
		signal::{killpg, Signal},
		wait::WaitPidFlag,
	},
	unistd::{setpgid, Pid},
};
use tracing::instrument;

use crate::ChildExitStatus;

use super::{StdChildWrapper, StdCommandWrap, StdCommandWrapper};

#[derive(Debug, Clone)]
pub struct ProcessGroup {
	leader: Pid,
}

impl ProcessGroup {
	pub fn leader() -> Self {
		Self {
			leader: Pid::from_raw(0),
		}
	}

	pub fn attach_to(leader: Pid) -> Self {
		Self { leader }
	}
}

#[derive(Debug)]
pub struct ProcessGroupChild {
	inner: Box<dyn StdChildWrapper>,
	exit_status: ChildExitStatus,
	pgid: Pid,
}

impl ProcessGroupChild {
	#[instrument(level = "debug")]
	pub(crate) fn new(inner: Box<dyn StdChildWrapper>, pgid: Pid) -> Self {
		Self {
			inner,
			exit_status: ChildExitStatus::Running,
			pgid,
		}
	}
}

impl StdCommandWrapper for ProcessGroup {
	#[instrument(level = "debug", skip(self))]
	fn pre_spawn(&mut self, command: &mut Command, _core: &StdCommandWrap) -> Result<()> {
		#[cfg(Std_unstable)]
		{
			command.process_group(self.leader.as_raw());
		}

		#[cfg(not(Std_unstable))]
		let leader = self.leader;
		unsafe {
			command.pre_exec(move || {
				setpgid(Pid::this(), leader)
					.map_err(Error::from)
					.map(|_| ())
			});
		}

		Ok(())
	}

	#[instrument(level = "debug", skip(self))]
	fn wrap_child(
		&mut self,
		inner: Box<dyn StdChildWrapper>,
		_core: &StdCommandWrap,
	) -> Result<Box<dyn StdChildWrapper>> {
		let pgid = Pid::from_raw(i32::try_from(inner.id()).expect("Command PID > i32::MAX"));

		Ok(Box::new(ProcessGroupChild::new(inner, pgid)))
	}
}

impl ProcessGroupChild {
	#[instrument(level = "debug", skip(self))]
	fn signal_imp(&self, sig: Signal) -> Result<()> {
		killpg(self.pgid, sig).map_err(Error::from)
	}

	#[instrument(level = "debug")]
	fn wait_imp(pgid: Pid, flag: WaitPidFlag) -> Result<ControlFlow<Option<ExitStatus>>> {
		// wait for processes in a loop until every process in this group has
		// exited (this ensures that we reap any zombies that may have been
		// created if the parent exited after spawning children, but didn't wait
		// for those children to exit)
		let mut parent_exit_status: Option<ExitStatus> = None;
		loop {
			// we can't use the safe wrapper directly because it doesn't return
			// the raw status, and we need it to convert to the std's ExitStatus
			let mut status: i32 = 0;
			match unsafe {
				libc::waitpid(-pgid.as_raw(), &mut status as *mut libc::c_int, flag.bits())
			} {
				0 => {
					// zero should only happen if WNOHANG was passed in,
					// and means that no processes have yet to exit
					return Ok(ControlFlow::Continue(()));
				}
				-1 => {
					match Errno::last() {
						Errno::ECHILD => {
							// no more children to reap; this is a graceful exit
							return Ok(ControlFlow::Break(parent_exit_status));
						}
						errno => {
							return Err(Error::from(errno));
						}
					}
				}
				pid => {
					// a process exited. was it the parent process that we
					// started? if so, collect the exit signal, otherwise we
					// reaped a zombie process and should continue looping
					if pgid == Pid::from_raw(pid) {
						parent_exit_status = Some(ExitStatus::from_raw(status));
					} else {
						// reaped a zombie child; keep looping
					}
				}
			};
		}
	}
}

impl StdChildWrapper for ProcessGroupChild {
	fn inner(&self) -> &Child {
		self.inner.inner()
	}
	fn inner_mut(&mut self) -> &mut Child {
		self.inner.inner_mut()
	}
	fn into_inner(self: Box<Self>) -> Child {
		self.inner.into_inner()
	}

	#[instrument(level = "debug", skip(self))]
	fn start_kill(&mut self) -> Result<()> {
		self.signal_imp(Signal::SIGKILL)
	}

	#[instrument(level = "debug", skip(self))]
	fn wait(&mut self) -> Result<ExitStatus> {
		if let ChildExitStatus::Exited(status) = &self.exit_status {
			return Ok(*status);
		}

		// always wait for parent to exit first, as by the time it does,
		// it's likely that all its children have already been reaped.
		let status = self.inner.wait()?;
		self.exit_status = ChildExitStatus::Exited(status);

		// nevertheless, now wait and make sure we reap all children.
		Self::wait_imp(self.pgid, WaitPidFlag::empty())?;
		Ok(status)
	}

	#[instrument(level = "debug", skip(self))]
	fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
		if let ChildExitStatus::Exited(status) = &self.exit_status {
			return Ok(Some(*status));
		}

		match Self::wait_imp(self.pgid, WaitPidFlag::WNOHANG)? {
			ControlFlow::Break(res) => {
				if let Some(status) = res {
					self.exit_status = ChildExitStatus::Exited(status);
				}
				Ok(res)
			}
			ControlFlow::Continue(()) => {
				let exited = self.inner.try_wait()?;
				if let Some(exited) = exited {
					self.exit_status = ChildExitStatus::Exited(exited);
				}
				Ok(exited)
			}
		}
	}

	fn signal(&self, sig: Signal) -> Result<()> {
		self.signal_imp(sig)
	}
}