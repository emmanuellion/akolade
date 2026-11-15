use std::{
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use gpui::{App, AppContext, Context, Entity, Window};
use gpui_terminal::{TerminalConfig, TerminalView};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// PTY stdin shared between the terminal view (keyboard) and the editor (Run).
#[derive(Clone)]
pub struct PtyWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// The terminal's foreground process group is not the shell's.
fn foreground_busy(master: &dyn MasterPty, shell: &dyn Child) -> bool {
    matches!((master.process_group_leader(), shell.process_id()), (Some(leader), Some(shell)) if leader as u32 != shell)
}

pub struct Terminal {
    pub view: Entity<TerminalView>,
    pub input: PtyWriter,
    child: Box<dyn Child + Send + Sync>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Its tab's name: "zsh 2".
    pub title: String,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Terminal {
    pub fn spawn(
        cwd: &Path,
        config: TerminalConfig,
        on_exit: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
        cx: &mut App,
    ) -> Result<Self> {
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system().openpty(size)?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(cmd)?;

        let input = PtyWriter(Arc::new(Mutex::new(pair.master.take_writer()?)));
        let reader = pair.master.try_clone_reader()?;
        let master = Arc::new(Mutex::new(pair.master));

        let (writer, resize) = (input.clone(), master.clone());
        let view = cx.new(|cx| {
            TerminalView::new(writer, reader, config, cx)
                .with_resize_callback(move |cols, rows| {
                    let size = PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 };
                    let _ = resize.lock().unwrap().resize(size);
                })
                .with_exit_callback(on_exit)
        });
        Ok(Self { view, input, child, master, title: String::new() })
    }

    /// A program runs in the foreground (a dev server…): what is typed goes to it, not to the shell.
    pub fn busy(&self) -> bool {
        foreground_busy(&**self.master.lock().unwrap(), &*self.child)
    }

    /// Types a command into the shell, as if the user did; false (nothing typed) when busy.
    pub fn run(&mut self, command: &str) -> bool {
        if self.busy() {
            return false;
        }
        let _ = self.input.write_all(format!("{command}\r").as_bytes());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A real shell in a real PTY: idle, then running a program in the foreground.
    #[test]
    fn a_running_program_makes_the_terminal_busy() {
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new("/bin/zsh");
        cmd.args(["-f", "-i"]);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        // Drain the output, or the shell blocks on a full PTY.
        std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!foreground_busy(&*pair.master, &*child), "idle shell");
        writer.write_all(b"sleep 3\r").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(foreground_busy(&*pair.master, &*child), "sleep runs in the foreground");
        let _ = child.kill();
    }
}
