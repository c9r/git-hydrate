//! Progress reporting on standard error, silent when it is not a terminal.

use std::io::IsTerminal;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// The set of progress bars a command draws.
pub struct Progress {
    multi: MultiProgress,
}

impl Progress {
    /// Creates a reporter that draws only when standard error is a terminal and `quiet` is false.
    pub fn new(quiet: bool) -> Progress {
        let multi = MultiProgress::new();
        if quiet || !std::io::stderr().is_terminal() {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        Progress { multi }
    }

    /// A bar counting bytes toward a total, labeled with a path.
    pub fn bytes(&self, label: &str, total: u64) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(total));
        bar.set_style(
            ProgressStyle::with_template("{msg:<48!} {bytes:>10} / {total_bytes:<10} {bytes_per_sec:>12} {eta:>6}")
                .expect("the progress template is valid"),
        );
        bar.set_message(label.to_string());
        bar
    }

    /// A spinner-free counter of items toward a total, for work without byte sizes.
    pub fn items(&self, label: &str, total: u64) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(total));
        bar.set_style(
            ProgressStyle::with_template("{msg:<48!} {pos:>8} / {len:<8}").expect("the progress template is valid"),
        );
        bar.set_message(label.to_string());
        bar
    }

    /// Prints a line above the bars without disturbing them.
    pub fn println(&self, line: &str) {
        if self.multi.is_hidden() {
            eprintln!("{line}");
        } else {
            let _ = self.multi.println(line);
        }
    }
}
